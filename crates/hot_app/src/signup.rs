//! Who may create an account (`app.signup`).

use hot::db::invite::{Invite, InviteStatus};
use hot::db::{BootstrapOperation, DatabasePool, DatabaseScope, EmailVerification};
use hot::val::Val;

/// `open` lets anyone create an account. `invite` admits only people with a
/// pending invitation for their email address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignupPolicy {
    Open,
    Invite,
}

impl SignupPolicy {
    pub fn from_conf(conf: &Val) -> Result<Self, String> {
        match conf
            .get_str_or_default("app.signup", "open")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "open" => Ok(Self::Open),
            "invite" => Ok(Self::Invite),
            other => Err(format!(
                "Unsupported app.signup {other:?}; expected open or invite"
            )),
        }
    }
}

/// Whether the sign-up form accepts people without an invitation. The form
/// proves mailbox control with a verification email, so a self-hosted
/// installation that cannot send email admits only invited users.
pub fn open_signup_available(conf: &Val) -> bool {
    matches!(SignupPolicy::from_conf(conf), Ok(SignupPolicy::Open))
        && verification_email_deliverable(conf)
}

/// Whether a verification email can reach anyone. Only a self-hosted
/// installation can be without a provider that delivers them, but no
/// installation queues one it would send from the wrong sender or with a link
/// that leads elsewhere.
pub fn verification_email_deliverable(conf: &Val) -> bool {
    if hot::product::is_self_host(conf) {
        hot::email::email_delivery_configured(conf, hot::email::EmailPurpose::App)
    } else {
        hot::email::email_render_status(conf).is_ok()
    }
}

/// Whether people whose email address is already verified may sign up
/// without an invitation. OAuth providers verify the address themselves, and
/// a pending verification email has already been delivered, so only the
/// policy limits them.
pub fn verified_signup_open(conf: &Val) -> bool {
    matches!(SignupPolicy::from_conf(conf), Ok(SignupPolicy::Open))
}

/// Whether `invite` can admit `email` to an invite-only installation: it is
/// for this address, still pending, and its organization would accept it
/// now. These are the checks accepting an invitation makes, apart from the
/// account itself, so sign-up never creates an account for an invitation it
/// then cannot apply.
pub async fn invite_admits(db: &DatabasePool, invite: &Invite, email: &str) -> bool {
    if !invite.email.eq_ignore_ascii_case(email)
        || invite.is_valid().is_err()
        || invite.invite_status_id != InviteStatus::Invited.as_id()
    {
        return false;
    }
    // The invitation is the authority to read its organization, as when it
    // is accepted.
    let org_db = db.with_scope(DatabaseScope::Organization {
        org_id: invite.org_id,
    });
    let Ok(org) = hot::db::org::Org::get_org_uncached(&org_db, &invite.org_id).await else {
        return false;
    };
    if org.parent_org_id.is_some() || !org.active || org.lifecycle_state != "active" {
        return false;
    }
    let seats = hot::db::Features::resolve_for_org(&org_db, &invite.org_id)
        .await
        .team_members();
    seats < 0
        || hot::db::org::OrgUser::count_active_members(&org_db, &invite.org_id)
            .await
            .is_ok_and(|members| members < i64::from(seats))
}

/// Whether a pending verification can still become an account on an
/// invite-only installation. Only the invitation it carries itself counts. It
/// holds the password of whoever asked for it, so an invitation sent to the
/// address later must not turn that request into the invited person's account.
/// They sign up through their invitation link, which ends the verification.
pub async fn verification_admitted(db: &DatabasePool, verification: &EmailVerification) -> bool {
    let Some(code) = verification
        .invite_code
        .as_deref()
        .filter(|code| !code.is_empty())
    else {
        return false;
    };
    let Ok(invite) = Invite::get_invite_by_code(db, code).await else {
        return false;
    };
    invite_admits(db, &invite, &verification.email).await
}

/// The invitation that admits a provider-verified `email` to an invite-only
/// installation: the one in the sign-up link when it is for this address,
/// otherwise a pending invitation sent to it. Sign-up then applies it.
pub async fn admitting_invite(
    db: &DatabasePool,
    email: &str,
    invite_code: Option<&str>,
) -> Result<Option<Invite>, String> {
    if let Some(code) = invite_code.filter(|code| !code.is_empty())
        && let Ok(invite) = Invite::get_invite_by_code(db, code).await
        && invite_admits(db, &invite, email).await
    {
        return Ok(Some(invite));
    }
    // Invitations are readable by address only under the membership lookup
    // scope.
    let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let invites = Invite::get_invites_by_email(&lookup, email)
        .await
        .map_err(|e| format!("Failed to check invitations: {}", e))?;
    for invite in invites {
        if invite_admits(db, &invite, email).await {
            return Ok(Some(invite));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_signup_needs_email_or_an_invitation() {
        let self_host =
            |extra: Val| hot::val!({"product": {"experience": "self-host"}}).merge(&extra);
        let smtp = hot::val!({"email": {
            "provider": "smtp", "from": "hot@example.com", "smtp": {"host": "smtp.example.com"}
        }});
        assert!(!open_signup_available(&self_host(hot::val!({}))));
        assert!(!verification_email_deliverable(&self_host(hot::val!({}))));
        assert!(verified_signup_open(&self_host(hot::val!({}))));
        assert!(open_signup_available(&self_host(smtp.clone())));
        assert!(verification_email_deliverable(&self_host(smtp.clone())));
        // A provider name alone does not deliver email.
        assert!(!open_signup_available(&self_host(
            hot::val!({"email": {"provider": "smtp"}})
        )));
        let invite = self_host(smtp.merge(&hot::val!({"app": {"signup": "invite"}})));
        assert!(!open_signup_available(&invite));
        assert!(!verified_signup_open(&invite));
        // Other experiences keep open sign-up without a configured provider.
        assert!(open_signup_available(&hot::val!({})));
        assert!(verification_email_deliverable(&hot::val!({})));
        assert!(SignupPolicy::from_conf(&hot::val!({"app": {"signup": "closed"}})).is_err());
    }
}

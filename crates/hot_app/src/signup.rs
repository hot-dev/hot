//! Who may create an account (`app.signup`).

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
        && (!hot::product::is_self_host(conf)
            || hot::email::email_delivery_configured(conf, hot::email::EmailPurpose::App))
}

/// OAuth providers verify the email address themselves, so only the policy
/// limits who can sign up through them.
pub fn oauth_signup_open(conf: &Val) -> bool {
    matches!(SignupPolicy::from_conf(conf), Ok(SignupPolicy::Open))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_signup_needs_email_or_an_invitation() {
        let self_host =
            |extra: Val| hot::val!({"product": {"experience": "self-host"}}).merge(&extra);
        assert!(!open_signup_available(&self_host(hot::val!({}))));
        assert!(oauth_signup_open(&self_host(hot::val!({}))));
        assert!(open_signup_available(&self_host(
            hot::val!({"email": {"provider": "smtp"}})
        )));
        let invite =
            self_host(hot::val!({"email": {"provider": "smtp"}, "app": {"signup": "invite"}}));
        assert!(!open_signup_available(&invite));
        assert!(!oauth_signup_open(&invite));
        // Other experiences keep open sign-up without a configured provider.
        assert!(open_signup_available(&hot::val!({})));
        assert!(SignupPolicy::from_conf(&hot::val!({"app": {"signup": "closed"}})).is_err());
    }
}

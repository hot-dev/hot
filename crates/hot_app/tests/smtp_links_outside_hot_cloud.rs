//! SMTP mail on an installation whose `HOT_ENV` would send its links to Hot
//! Cloud's app, or whose links name only the machine they are opened on. The
//! app refuses to write its own mail, because its links carry codes; the worker
//! sends what the app wrote, and alerts with a warning. This binary has a single
//! test because it changes the process's environment.

use hot::db::DatabasePool;
use hot::email::{
    EmailPurpose, EmailSender, email_delivery_status, email_render_status, own_app_address,
};
use hot_app::email::{AppEmailEnqueuer, AppEmailSender};
use hot_app::signup::{open_signup_available, verification_email_deliverable};
use std::sync::Arc;

/// `hot conf generate all` writes the first of these when `HOT_APP_URL` is unset.
const LOOPBACK_ADDRESSES: [&str; 3] = [
    "http://localhost:4680",
    "http://127.0.0.1:4680",
    "http://[::1]:4680",
];

#[test]
fn smtp_needs_this_installations_address_in_production_and_staging() {
    let smtp = hot::val!({
        "product": {"experience": "self-host"},
        "email": {"provider": "smtp", "from": "hot@acme.com", "smtp": {"host": "smtp.acme.com"}},
    });
    let own_address = smtp.merge(&hot::val!({"email": {"app-url": "https://hot.acme.com"}}));
    let at = |app_url: &str| smtp.merge(&hot::val!({"email": {"app-url": app_url}}));
    // A sender's SMTP connection pool needs a runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _runtime = runtime.enter();

    // SAFETY: nothing else runs in this binary, for the whole of this test.
    unsafe { std::env::remove_var("HOT_APP_URL") };
    for hot_env in ["production", "staging"] {
        // SAFETY: as above.
        unsafe { std::env::set_var("HOT_ENV", hot_env) };
        let reason = email_delivery_status(&smtp, EmailPurpose::App).unwrap_err();
        assert!(reason.contains("email.app-url or HOT_APP_URL"), "{reason}");
        assert_eq!(
            email_delivery_status(&own_address, EmailPurpose::App),
            Ok(())
        );
        // A worker without an address of its own sends the links the app wrote,
        // and `hot db redeploy`'s mail, which has none. Alerts go out regardless.
        for conf in [&smtp, &own_address] {
            assert!(EmailSender::from_conf(conf).is_available());
            assert_eq!(email_delivery_status(conf, EmailPurpose::Alerts), Ok(()));
            assert!(EmailSender::alerts_from_conf(conf).is_available());
        }
        // Nor does a worker need a sender of its own for the mail the app wrote,
        // though it can't send alerts without one.
        let relay_only = hot::val!({"email": {
            "provider": "smtp", "smtp": {"host": "smtp.acme.com"}
        }});
        assert!(EmailSender::from_conf(&relay_only).is_available());
        assert!(!EmailSender::alerts_from_conf(&relay_only).is_available());
        // It stays invite-only, showing admins links on their own address.
        assert!(!open_signup_available(&smtp));
        assert!(!verification_email_deliverable(&smtp));
        assert!(open_signup_available(&own_address));
        // `hot admin bootstrap` won't send a person to the fallback, either.
        assert_eq!(own_app_address(&smtp), None);
        assert_eq!(
            own_app_address(&own_address).as_deref(),
            Some("https://hot.acme.com")
        );

        // No recipient can follow a link to this machine, either.
        for loopback in LOOPBACK_ADDRESSES {
            let conf = at(loopback);
            assert_eq!(own_app_address(&conf), None);
            let reason = email_delivery_status(&conf, EmailPurpose::App).unwrap_err();
            assert!(reason.contains("email.app-url or HOT_APP_URL"), "{reason}");
            assert!(reason.contains(loopback), "{reason}");
            assert_eq!(email_delivery_status(&conf, EmailPurpose::Alerts), Ok(()));
            assert!(!open_signup_available(&conf));
            assert!(!verification_email_deliverable(&conf));
        }
        // SAFETY: as above.
        unsafe { std::env::set_var("HOT_APP_URL", "http://localhost:4680") };
        assert!(email_delivery_status(&smtp, EmailPurpose::App).is_err());

        // SAFETY: as above.
        unsafe { std::env::set_var("HOT_APP_URL", "https://hot.acme.com") };
        assert_eq!(email_delivery_status(&smtp, EmailPurpose::App), Ok(()));
        assert!(open_signup_available(&smtp));
        assert_eq!(
            own_app_address(&smtp).as_deref(),
            Some("https://hot.acme.com")
        );
        // A blank value names nothing.
        // SAFETY: as above.
        unsafe { std::env::set_var("HOT_APP_URL", " ") };
        assert!(email_delivery_status(&smtp, EmailPurpose::App).is_err());
        // SAFETY: as above.
        unsafe { std::env::remove_var("HOT_APP_URL") };
    }

    mail_is_refused_where_it_is_written(&runtime);

    // Any other value but development is a deployment too, and its fallback
    // address is `localhost`.
    // SAFETY: as above.
    unsafe { std::env::set_var("HOT_ENV", "prod") };
    assert!(email_delivery_status(&smtp, EmailPurpose::App).is_err());
    assert_eq!(
        email_delivery_status(&own_address, EmailPurpose::App),
        Ok(())
    );

    // SAFETY: as above.
    unsafe { std::env::set_var("HOT_ENV", "development") };
    assert_eq!(email_delivery_status(&smtp, EmailPurpose::App), Ok(()));
    assert_eq!(
        own_app_address(&smtp).as_deref(),
        Some("http://localhost:4680")
    );
    for loopback in LOOPBACK_ADDRESSES {
        assert_eq!(
            email_delivery_status(&at(loopback), EmailPurpose::App),
            Ok(())
        );
    }
    // SAFETY: as above.
    unsafe { std::env::remove_var("HOT_ENV") };
    assert_eq!(email_delivery_status(&smtp, EmailPurpose::App), Ok(()));
    assert!(open_signup_available(&smtp));
}

/// An app that names neither a sender nor an address writes no mail, whichever
/// process holds the delivery settings. `HOT_ENV` is production here.
fn mail_is_refused_where_it_is_written(runtime: &tokio::runtime::Runtime) {
    // SAFETY: nothing else runs in this binary, for the whole of the test.
    unsafe { std::env::set_var("HOT_ENV", "production") };
    let queued = |conf: hot::val::Val, to: &'static str| {
        runtime.block_on(async move {
            let db = Arc::new(hot::db::test_db().await);
            let enqueuer = AppEmailEnqueuer::from_conf(db.clone(), &conf);
            let sent = enqueuer
                .send_destination_verification_email(to, "Acme", "Ops", "the-bearer-token")
                .await;
            let DatabasePool::Sqlite(pool) = db.as_ref() else {
                panic!("test_db is SQLite");
            };
            let rows: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT from_address, text_body FROM email_queue WHERE to_address = ?",
            )
            .bind(to)
            .fetch_all(pool)
            .await
            .unwrap();
            (sent.map_err(|error| error.to_string()), rows)
        })
    };

    for experience in ["self-host", "local-dev"] {
        let bare = hot::val!({"product": {"experience": experience}});
        let reason = email_render_status(&bare).unwrap_err();
        assert!(reason.contains("HOT_EMAIL_FROM"), "{reason}");
        let (sent, rows) = queued(bare.clone(), "bare@example.com");
        assert!(sent.unwrap_err().contains("HOT_EMAIL_FROM"));
        assert!(rows.is_empty(), "{rows:?}");
        assert!(!verification_email_deliverable(&bare));

        // Its own sender isn't enough while the link would lead to Hot Cloud.
        let sender = bare.merge(&hot::val!({"email": {"from": "hot@acme.com"}}));
        let (sent, rows) = queued(sender.clone(), "sender@example.com");
        assert!(sent.unwrap_err().contains("HOT_APP_URL"));
        assert!(rows.is_empty(), "{rows:?}");

        // Nor is an address, with Hot Cloud's sender.
        let address = bare.merge(&hot::val!({"email": {"app-url": "https://hot.acme.com"}}));
        let (sent, rows) = queued(address.clone(), "address@example.com");
        assert!(sent.unwrap_err().contains("HOT_EMAIL_FROM"));
        assert!(rows.is_empty(), "{rows:?}");

        // With both it queues, with no delivery settings: the worker may hold them.
        let own = sender.merge(&address);
        let (sent, rows) = queued(own, "own@example.com");
        assert_eq!(sent, Ok(()));
        let [(from_address, text_body)] = rows.as_slice() else {
            panic!("one email is queued: {rows:?}");
        };
        assert!(from_address.contains("hot@acme.com"), "{from_address}");
        let text_body = text_body.as_deref().unwrap();
        assert!(
            text_body
                .contains("https://hot.acme.com/verify-alert-destination?token=the-bearer-token"),
            "{text_body}"
        );
    }

    // Hot Cloud sends as itself.
    let hot_cloud = hot::val!({"product": {"experience": "hot-cloud"}});
    let (sent, rows) = queued(hot_cloud, "cloud@example.com");
    assert_eq!(sent, Ok(()));
    assert_eq!(rows.len(), 1);

    // A local development run queues on what it has, with `HOT_ENV` unset or set.
    for hot_env in [None, Some("development")] {
        // SAFETY: as above.
        unsafe {
            match hot_env {
                Some(hot_env) => std::env::set_var("HOT_ENV", hot_env),
                None => std::env::remove_var("HOT_ENV"),
            }
        }
        let (sent, rows) = queued(hot::val!({}), "local@example.com");
        assert_eq!(sent, Ok(()));
        assert_eq!(rows.len(), 1);
    }
}

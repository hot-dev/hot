//! Real HTTP routing, restricted database writes, and queue/result boundaries.
//! The lifecycle fixture owns the database/roles; this helper stands in only for
//! the worker's completion publisher, after inspecting the queued execution.

use super::get_event_queue;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use hot::{
    db::{DatabasePool, DatabaseScope, ScopedPgPool},
    lang::event::EventMessage,
    queue::Queue,
    stream::{StreamEvent, StreamPubSub, StreamPublisher},
    val::Val,
};
use serde_json::json;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(crate) struct Tenant<'a> {
    pub org_id: Uuid,
    pub env_id: Uuid,
    pub slug: &'a str,
    pub env_name: &'a str,
    pub api_token: &'a str,
    pub service_token: &'a str,
    pub deployment_creator: Uuid,
}

fn environment(db: &DatabasePool, tenant: Tenant<'_>) -> ScopedPgPool {
    let DatabasePool::Postgres(pool) = db else {
        panic!("webhook lifecycle requires restricted PostgreSQL");
    };
    pool.with_scope(DatabaseScope::Environment {
        org_id: tenant.org_id,
        env_id: tenant.env_id,
    })
}

struct Deployment {
    build: Uuid,
    project: Uuid,
    public_token: String,
    public_uri: String,
    private_uri: String,
    malformed_auth_uri: String,
}

async fn deployment(db: &DatabasePool, tenant: Tenant<'_>) -> Deployment {
    let pool = environment(db, tenant);
    let creator = tenant.deployment_creator;
    let project = Uuid::new_v4();
    let build = Uuid::new_v4();
    sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES($1,$2,'webhook-lifecycle',$3)")
        .bind(project).bind(tenant.env_id).bind(creator).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO build(build_id,project_id,hash,size,deployed,runtime_status,created_by_user_id) VALUES($1,$2,$3,1,true,'ready',$4)")
        .bind(build).bind(project).bind(build.to_string()).bind(creator).execute(&pool).await.unwrap();
    let public = Uuid::new_v4();
    let private = Uuid::new_v4();
    let malformed_auth = Uuid::new_v4();
    for (endpoint, path, auth) in [
        (public, "/public", "none"),
        (private, "/private", "required"),
        (malformed_auth, "/malformed", "Required"),
    ] {
        sqlx::query("INSERT INTO webhook(webhook_id,build_id,service,path,method,ns,var,name,meta) VALUES($1,$2,'lifecycle',$3,'POST','::lifecycle','call','call',$4)")
            .bind(endpoint).bind(build).bind(path).bind(json!({"webhook":{"auth":auth}})).execute(&pool).await.unwrap();
    }
    let public_token = hot::db::webhook::uuid_short(&public);
    Deployment {
        build,
        project,
        public_uri: format!(
            "/webhook/{}/{}/lifecycle/public/{public_token}",
            tenant.slug, tenant.env_name
        ),
        private_uri: format!(
            "/webhook/{}/{}/lifecycle/private/{}",
            tenant.slug,
            tenant.env_name,
            hot::db::webhook::uuid_short(&private)
        ),
        malformed_auth_uri: format!(
            "/webhook/{}/{}/lifecycle/malformed/{}",
            tenant.slug,
            tenant.env_name,
            hot::db::webhook::uuid_short(&malformed_auth)
        ),
        public_token,
    }
}

fn request(uri: &str, token: Option<&str>, host: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", host)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.body(Body::from(r#"{"message":"scoped"}"#)).unwrap()
}

struct Requests<'a> {
    app: &'a Router,
    db: &'a DatabasePool,
    conf: &'a Val,
    pubsub: &'a StreamPubSub,
    creator: Uuid,
    child: Tenant<'a>,
    sibling: Tenant<'a>,
}

impl Requests<'_> {
    async fn event_counts(&self) -> (i64, i64) {
        let child = sqlx::query_scalar("SELECT count(*) FROM event")
            .fetch_one(&environment(self.db, self.child))
            .await
            .unwrap();
        let sibling = sqlx::query_scalar("SELECT count(*) FROM event")
            .fetch_one(&environment(self.db, self.sibling))
            .await
            .unwrap();
        (child, sibling)
    }

    async fn denied(&self, uri: &str, token: Option<&str>, host: &str, status: StatusCode) {
        let before = self.event_counts().await;
        let queue = get_event_queue(self.conf).unwrap();
        let queued = queue.len().await.unwrap();
        let response = self
            .app
            .clone()
            .oneshot(request(uri, token, host))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "unexpected denial for {uri}");
        assert_eq!(
            self.event_counts().await,
            before,
            "denied requests must not persist work"
        );
        assert_eq!(
            queue.len().await.unwrap(),
            queued,
            "denied requests must not enqueue work"
        );
    }

    async fn accepted(
        &self,
        tenant: Tenant<'_>,
        deployment: &Deployment,
        uri: &str,
        token: Option<&str>,
        host: &str,
    ) {
        let queue = get_event_queue(self.conf).unwrap();
        let mut response = tokio::spawn(self.app.clone().oneshot(request(uri, token, host)));
        let message = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(message) = queue.dequeue().await.unwrap() {
                    break message;
                }
                tokio::select! {
                    completed = &mut response => {
                        let completed = completed.unwrap().unwrap();
                        let status = completed.status();
                        let body = to_bytes(completed.into_body(),1024*1024).await.unwrap();
                        panic!("valid webhook returned before enqueueing: {status}: {}",String::from_utf8_lossy(&body));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
        })
        .await
        .expect("valid restricted-role webhook must enqueue work");
        let message = EventMessage::try_from(message).unwrap();
        let context = &message.body.execution_context;
        let expected_creator = if token.is_some() {
            self.creator
        } else {
            tenant.deployment_creator
        };
        assert_eq!(context.org_id, Some(tenant.org_id));
        assert_eq!(context.env_id, Some(tenant.env_id));
        assert_eq!(context.build_id, Some(deployment.build));
        // Queue messages deliberately omit hydrated project metadata and event
        // data. The worker validates this routing envelope against the scoped
        // durable event/build before filling the execution context.
        assert_eq!(context.project_id, None);
        assert_eq!(context.user_id, Some(expected_creator));
        assert_eq!(message.body.event.env_id, tenant.env_id);
        assert_eq!(
            message.body.event.target_project_id,
            Some(deployment.project)
        );
        assert_eq!(
            message.body.event.event_data.get_str("event_id"),
            message.id.to_string()
        );

        let pool = environment(self.db, tenant);
        let persisted: (Uuid, Uuid, Uuid, serde_json::Value) = sqlx::query_as(
            "SELECT env_id,stream_id,created_by_user_id,event_data FROM event WHERE event_id=$1",
        )
        .bind(message.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (persisted.0, persisted.1, persisted.2),
            (tenant.env_id, context.stream_id, expected_creator)
        );
        assert_eq!(
            persisted.3["_hot_target_build_id"],
            deployment.build.to_string(),
            "the worker must hydrate the exact selected deployment"
        );
        let mut hydrated: Val = serde_json::from_value(persisted.3.clone()).unwrap();
        hot::webhook_url::restore_event_data_original_urls(
            &mut hydrated,
            &message.body.event.event_data,
        );
        let caller = hydrated.get("caller").unwrap();
        assert_eq!(
            caller.get_str("original-url"),
            format!("https://{host}{uri}")
        );
        assert!(
            !persisted
                .3
                .to_string()
                .contains(uri.rsplit('/').next().unwrap()),
            "persisted event must redact capability token"
        );
        let foreign = if tenant.org_id == self.child.org_id {
            self.sibling
        } else {
            self.child
        };
        let hidden: i64 = sqlx::query_scalar("SELECT count(*) FROM event WHERE event_id=$1")
            .bind(message.id)
            .fetch_one(&environment(self.db, foreign))
            .await
            .unwrap();
        assert_eq!(hidden, 0, "sibling must not read webhook event");

        // A completion with the right stream ID but a sibling environment must
        // not satisfy the waiting HTTP request.
        self.pubsub
            .publish(StreamEvent::RunStop {
                run_id: context.run_id,
                env_id: foreign.env_id,
                stream_id: context.stream_id,
                event_id: Some(message.id),
                result: Some(json!("foreign result")),
            })
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut response)
                .await
                .is_err()
        );
        let expected = json!({"org":tenant.org_id,"build":deployment.build});
        self.pubsub
            .publish(StreamEvent::RunStop {
                run_id: context.run_id,
                env_id: tenant.env_id,
                stream_id: context.stream_id,
                event_id: Some(message.id),
                result: Some(expected.clone()),
            })
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            expected
        );
    }
}

/// Returns the public child URL so the surrounding lifecycle test can assert
/// suspension/archive also reject anonymous capabilities without creating work.
pub(crate) async fn exercise(
    app: &Router,
    db: &DatabasePool,
    conf: &Val,
    pubsub: &StreamPubSub,
    creator: Uuid,
    child: Tenant<'_>,
    sibling: Tenant<'_>,
) -> String {
    assert_ne!(creator, child.deployment_creator);
    let child_deployment = deployment(db, child).await;
    let sibling_deployment = deployment(db, sibling).await;
    let requests = Requests {
        app,
        db,
        conf,
        pubsub,
        creator,
        child,
        sibling,
    };
    let visible_users: i64 = sqlx::query_scalar("SELECT count(*) FROM hot.\"user\"")
        .fetch_one(&environment(db, child))
        .await
        .unwrap();
    assert_eq!(
        visible_users, 0,
        "child webhook must not require access to the root member directory"
    );

    requests
        .accepted(
            child,
            &child_deployment,
            &child_deployment.public_uri,
            None,
            "localhost",
        )
        .await;
    requests
        .accepted(
            child,
            &child_deployment,
            &child_deployment.private_uri,
            Some(child.api_token),
            "localhost",
        )
        .await;
    requests
        .accepted(
            child,
            &child_deployment,
            &child_deployment.private_uri,
            Some(child.service_token),
            "localhost",
        )
        .await;
    requests
        .accepted(
            sibling,
            &sibling_deployment,
            &sibling_deployment.public_uri,
            None,
            "localhost",
        )
        .await;
    requests
        .denied(
            &child_deployment.private_uri,
            None,
            "localhost",
            StatusCode::UNAUTHORIZED,
        )
        .await;
    requests
        .denied(
            &child_deployment.malformed_auth_uri,
            None,
            "localhost",
            StatusCode::UNAUTHORIZED,
        )
        .await;
    requests
        .accepted(
            child,
            &child_deployment,
            &child_deployment.malformed_auth_uri,
            Some(child.service_token),
            "localhost",
        )
        .await;
    requests
        .denied(
            &child_deployment.private_uri,
            Some(sibling.api_token),
            "localhost",
            StatusCode::FORBIDDEN,
        )
        .await;
    requests
        .denied(
            &child_deployment.private_uri,
            Some(sibling.service_token),
            "localhost",
            StatusCode::FORBIDDEN,
        )
        .await;
    requests
        .denied(
            &child_deployment.public_uri.replace(
                &child_deployment.public_token,
                &sibling_deployment.public_token,
            ),
            None,
            "localhost",
            StatusCode::NOT_FOUND,
        )
        .await;

    // Exercise real domain middleware and a warm positive cache. A sibling
    // capability still cannot route to the child's deployment, and deleting
    // the route immediately prevents further work.
    let host = format!("{}.example.test", Uuid::new_v4().simple());
    let pool = environment(db, child);
    sqlx::query("INSERT INTO domain(domain_id,env_id,domain,verified_at) VALUES($1,$2,$3,now())")
        .bind(Uuid::new_v4())
        .bind(child.env_id)
        .bind(&host)
        .execute(&pool)
        .await
        .unwrap();
    let domain_uri = format!(
        "/webhook/lifecycle/public/{}",
        child_deployment.public_token
    );
    requests
        .accepted(child, &child_deployment, &domain_uri, None, &host)
        .await;
    requests
        .denied(
            &format!(
                "/webhook/lifecycle/public/{}",
                sibling_deployment.public_token
            ),
            None,
            &host,
            StatusCode::NOT_FOUND,
        )
        .await;
    sqlx::query("UPDATE domain SET deleted_at=now() WHERE domain=$1")
        .bind(&host)
        .execute(&pool)
        .await
        .unwrap();
    requests
        .denied(&domain_uri, None, &host, StatusCode::NOT_FOUND)
        .await;
    assert!(get_event_queue(conf).unwrap().is_empty().await.unwrap());
    child_deployment.public_uri
}

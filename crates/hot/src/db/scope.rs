//! Trusted database authority, kept separate from request payloads and Hot ctx.
//!
//! A scope is established only after authentication and action authorization.
//! Tokio does not inherit task locals into spawned tasks: use `with_scope` on
//! the database handle or explicitly carry the scope when spawning work.

use std::future::Future;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapOperation {
    Authentication,
    Routing,
    Membership,
    Onboarding,
    Health,
    /// An inbound billing-provider callback before it is resolved to one
    /// account. Grants no platform rows; providers narrow to
    /// `Maintenance { Billing, Some(root) }` (see `Org::billing_scope`).
    BillingWebhook,
}

impl BootstrapOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Routing => "routing",
            Self::Membership => "membership",
            Self::Onboarding => "onboarding",
            Self::Health => "health",
            Self::BillingWebhook => "billing_webhook",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceOperation {
    Queue,
    Scheduler,
    Retention,
    Billing,
    BlobGc,
    Alerts,
    Email,
    OrganizationProvisioning,
    StoreProvisioning,
}

impl MaintenanceOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Scheduler => "scheduler",
            Self::Retention => "retention",
            Self::Billing => "billing",
            Self::BlobGc => "blob_gc",
            Self::Alerts => "alerts",
            Self::Email => "email",
            Self::OrganizationProvisioning => "organization_provisioning",
            Self::StoreProvisioning => "store_provisioning",
        }
    }
}

/// Backend-created authority. This deliberately has no Deserialize impl.
/// Organization scope does not grant execution-table access in its envs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatabaseScope {
    /// Authenticated user's own profile and notification preferences only.
    User {
        user_id: Uuid,
    },
    Environment {
        org_id: Uuid,
        env_id: Uuid,
    },
    Organization {
        org_id: Uuid,
    },
    /// Both lists are exact allowlists. Listing an org does not implicitly add
    /// all its environments. PostgreSQL also makes this transaction read-only.
    ReadSet {
        org_ids: Vec<Uuid>,
        env_ids: Vec<Uuid>,
    },
    Bootstrap(BootstrapOperation),
    Maintenance {
        operation: MaintenanceOperation,
        org_id: Option<Uuid>,
    },
}

impl DatabaseScope {
    pub fn validate(&self) -> Result<(), sqlx::Error> {
        let invalid = match self {
            Self::User { user_id } => user_id.is_nil(),
            Self::Environment { org_id, env_id } => org_id.is_nil() || env_id.is_nil(),
            Self::Organization { org_id } => org_id.is_nil(),
            Self::ReadSet { org_ids, env_ids } => {
                org_ids.is_empty() || org_ids.iter().chain(env_ids).any(Uuid::is_nil)
            }
            Self::Bootstrap(_) => false,
            Self::Maintenance { operation, org_id } => {
                org_id.is_some_and(|id| id.is_nil())
                    || (matches!(
                        operation,
                        MaintenanceOperation::Billing
                            | MaintenanceOperation::OrganizationProvisioning
                    ) && org_id.is_none())
            }
        };
        if invalid {
            return Err(sqlx::Error::Protocol("Invalid database scope".into()));
        }
        Ok(())
    }

    /// Apply trusted authority inside an already-open transaction. This is used
    /// to retain an account admission lock while narrowing to the exact resource
    /// owner before mutation. Callers must not derive this authority from ctx.
    /// A read-only transaction stays read-only even if its scope is changed.
    pub async fn apply_to_connection(
        &self,
        connection: &mut sqlx::PgConnection,
    ) -> Result<(), sqlx::Error> {
        self.validate()?;
        let (mode, org, env, orgs, envs, operation, user) = self.settings();
        // One function call: it verifies environment ownership once for the
        // policies, makes read sets read-only, and (unlike a direct set_config)
        // does not pin sessions behind connection proxies such as RDS Proxy.
        sqlx::query("SELECT hot.apply_scope($1, $2, $3, $4, $5, $6, $7)")
            .bind(mode)
            .bind(org)
            .bind(env)
            .bind(orgs)
            .bind(envs)
            .bind(operation)
            .bind(user)
            .execute(connection)
            .await?;
        Ok(())
    }

    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadSet { .. })
    }

    pub(crate) fn settings(&self) -> (String, String, String, String, String, String, String) {
        let mut mode = String::new();
        let mut user = String::new();
        let mut org = String::new();
        let mut env = String::new();
        let mut orgs = String::from("[]");
        let mut envs = String::from("[]");
        let mut operation = String::new();
        match self {
            Self::User { user_id } => {
                mode.push_str("user");
                user = user_id.to_string();
            }
            Self::Environment { org_id, env_id } => {
                mode.push_str("environment");
                org = org_id.to_string();
                env = env_id.to_string();
            }
            Self::Organization { org_id } => {
                mode.push_str("organization");
                org = org_id.to_string();
            }
            Self::ReadSet { org_ids, env_ids } => {
                mode.push_str("read_set");
                orgs = serde_json::to_string(org_ids).expect("UUID lists serialize");
                envs = serde_json::to_string(env_ids).expect("UUID lists serialize");
            }
            Self::Bootstrap(op) => {
                mode.push_str("bootstrap");
                operation.push_str(op.as_str());
            }
            Self::Maintenance {
                operation: op,
                org_id,
            } => {
                mode.push_str("maintenance");
                operation.push_str(op.as_str());
                org = org_id.map(|id| id.to_string()).unwrap_or_default();
            }
        }
        (mode, org, env, orgs, envs, operation, user)
    }
}

tokio::task_local! {
    static DATABASE_SCOPE: DatabaseScope;
}

pub fn current_database_scope() -> Option<DatabaseScope> {
    DATABASE_SCOPE.try_with(Clone::clone).ok()
}

pub async fn with_database_scope<F: Future>(scope: DatabaseScope, future: F) -> F::Output {
    DATABASE_SCOPE.scope(scope, future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scope_nesting_restores_and_spawn_does_not_inherit() {
        assert!(current_database_scope().is_none());
        let outer = DatabaseScope::Organization {
            org_id: Uuid::new_v4(),
        };
        with_database_scope(outer.clone(), async {
            assert_eq!(current_database_scope(), Some(outer.clone()));
            with_database_scope(
                DatabaseScope::Bootstrap(BootstrapOperation::Health),
                async {
                    assert_ne!(current_database_scope(), Some(outer.clone()));
                },
            )
            .await;
            assert_eq!(current_database_scope(), Some(outer));
            assert!(
                tokio::spawn(async { current_database_scope() })
                    .await
                    .unwrap()
                    .is_none()
            );
        })
        .await;
        assert!(current_database_scope().is_none());
    }

    #[test]
    fn invalid_scope_cannot_use_nil_or_missing_account_as_wildcard() {
        assert!(
            DatabaseScope::User {
                user_id: Uuid::nil()
            }
            .validate()
            .is_err()
        );
        assert!(
            DatabaseScope::User {
                user_id: Uuid::new_v4()
            }
            .validate()
            .is_ok()
        );
        assert!(
            DatabaseScope::Environment {
                org_id: Uuid::new_v4(),
                env_id: Uuid::nil()
            }
            .validate()
            .is_err()
        );
        assert!(
            DatabaseScope::ReadSet {
                org_ids: vec![],
                env_ids: vec![]
            }
            .validate()
            .is_err()
        );
        assert!(
            DatabaseScope::Maintenance {
                operation: MaintenanceOperation::Billing,
                org_id: None
            }
            .validate()
            .is_err()
        );
    }
}

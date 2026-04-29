//! Domain model for custom domain mappings.
//!
//! Allows Hot Dev customers to map their own domains (e.g., mcp.example.com)
//! to their Hot Dev environments. Domain ownership is proven via provider DNS
//! validation, and TLS/routing are delegated to the configured domain provider.
//!
//! `org_id` is derived through `env_id` → `env.org_id` when needed (not stored).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use std::error::Error;
use std::fmt;
use uuid::Uuid;

// ============================================================================
// Error Type
// ============================================================================

#[derive(Debug)]
pub enum DomainError {
    Database(sqlx::Error),
    NotFound,
    AlreadyExists,
    /// The domain was recently removed and is still being cleaned up.
    PendingDeletion,
    NotVerified,
    InvalidDomain,
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainError::Database(e) => write!(f, "Database error: {}", e),
            DomainError::NotFound => write!(f, "Domain not found"),
            DomainError::AlreadyExists => write!(f, "Domain already registered"),
            DomainError::PendingDeletion => write!(
                f,
                "This domain was recently removed and is still being cleaned up. Please wait a few minutes and try again."
            ),
            DomainError::NotVerified => write!(f, "Domain ownership not verified"),
            DomainError::InvalidDomain => write!(f, "Invalid domain name"),
        }
    }
}

impl Error for DomainError {}

impl From<sqlx::Error> for DomainError {
    fn from(error: sqlx::Error) -> Self {
        DomainError::Database(error)
    }
}

// ============================================================================
// Domain Status Enum
// ============================================================================

/// High-level provisioning status for a custom domain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainStatus {
    /// Certificate requested, awaiting DNS validation by customer.
    PendingValidation,
    /// Certificate issued (domain ownership proven), routing not yet created.
    Validated,
    /// Provider routing target created, awaiting deployment.
    Provisioning,
    /// Provider routing target deployed and serving traffic.
    Active,
    /// Soft-deleted, awaiting provider resource cleanup by the worker.
    PendingDeletion,
}

impl fmt::Display for DomainStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainStatus::PendingValidation => write!(f, "pending_validation"),
            DomainStatus::Validated => write!(f, "validated"),
            DomainStatus::Provisioning => write!(f, "provisioning"),
            DomainStatus::Active => write!(f, "active"),
            DomainStatus::PendingDeletion => write!(f, "deleting"),
        }
    }
}

/// Result of a DNS CNAME check for a custom domain.
#[derive(Debug, Clone, PartialEq)]
pub enum CnameCheckResult {
    /// CNAME matches the expected routing target.
    Ok,
    /// No CNAME record found (DNS lookup failed or no records).
    Missing,
    /// CNAME exists but points to the wrong value (e.g., old distribution).
    Wrong(String),
    /// No routing target configured yet.
    NoDist,
}

// ============================================================================
// Domain Struct
// ============================================================================

/// Column list used in all SELECT queries — kept in sync with the struct fields.
const DOMAIN_COLUMNS: &str = r#"
    domain_id, env_id, domain,
    verified_at, tls_provisioned_at, created_at,
    certificate_ref, validation_cname_name, validation_cname_value,
    routing_ref, routing_domain, deleted_at, provisioning_error
"#;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Domain {
    pub domain_id: Uuid,
    pub env_id: Uuid,
    pub domain: String,
    pub verified_at: Option<DateTime<Utc>>,
    pub tls_provisioned_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    // -- Provider-managed certificate/routing fields --
    /// Provider certificate identifier.
    pub certificate_ref: Option<String>,
    /// The CNAME record name required for DNS validation.
    pub validation_cname_name: Option<String>,
    /// The CNAME record value required for DNS validation.
    pub validation_cname_value: Option<String>,
    /// Provider routing identifier.
    pub routing_ref: Option<String>,
    /// Provider routing domain.
    pub routing_domain: Option<String>,
    /// When set, the domain is soft-deleted and awaiting provider cleanup.
    pub deleted_at: Option<DateTime<Utc>>,
    /// User-facing error when provider provisioning fails (e.g., stale CNAME conflict).
    pub provisioning_error: Option<String>,
}

impl Domain {
    // ========================================================================
    // Status & lifecycle
    // ========================================================================

    /// Compute the high-level provisioning status from DB fields.
    pub fn status(&self) -> DomainStatus {
        if self.deleted_at.is_some() {
            DomainStatus::PendingDeletion
        } else if self.tls_provisioned_at.is_some() {
            DomainStatus::Active
        } else if self.routing_ref.is_some() {
            DomainStatus::Provisioning
        } else if self.verified_at.is_some() {
            DomainStatus::Validated
        } else {
            DomainStatus::PendingValidation
        }
    }

    /// Check if the domain has been verified (certificate issued).
    pub fn is_verified(&self) -> bool {
        self.verified_at.is_some()
    }

    /// Check if TLS/routing has been provisioned.
    pub fn is_tls_provisioned(&self) -> bool {
        self.tls_provisioned_at.is_some()
    }

    /// Check if the domain is fully ready for traffic.
    pub fn is_ready(&self) -> bool {
        self.is_verified() && self.is_tls_provisioned()
    }

    /// Whether this domain has certificate provisioning data.
    pub fn has_certificate(&self) -> bool {
        self.certificate_ref.is_some()
    }

    /// Whether this domain has provider routing data.
    pub fn has_routing(&self) -> bool {
        self.routing_ref.is_some()
    }

    /// Check if the domain's DNS CNAME points to the expected routing target.
    /// Returns Ok(true) if the CNAME matches, Ok(false) if it doesn't or isn't set,
    /// and Err on lookup failure.
    pub async fn check_domain_cname(&self) -> Result<CnameCheckResult, String> {
        let expected = match &self.routing_domain {
            Some(d) => d,
            None => return Ok(CnameCheckResult::NoDist),
        };

        use hickory_resolver::TokioResolver;
        use hickory_resolver::proto::rr::RecordType;

        let resolver = TokioResolver::builder_tokio()
            .map_err(|e| format!("DNS resolver error: {}", e))?
            .build()
            .map_err(|e| format!("DNS resolver error: {}", e))?;

        match resolver
            .lookup(&format!("{}.", self.domain), RecordType::CNAME)
            .await
        {
            Ok(lookup) => {
                let expected_normalized = expected.trim_end_matches('.').to_lowercase();
                for record in lookup.answers() {
                    let data_str = record.data.to_string();
                    let cname_normalized = data_str.trim_end_matches('.').to_lowercase();
                    if cname_normalized == expected_normalized {
                        return Ok(CnameCheckResult::Ok);
                    }
                }
                let actual = lookup
                    .answers()
                    .first()
                    .map(|r| r.data.to_string().trim_end_matches('.').to_lowercase())
                    .unwrap_or_default();
                Ok(CnameCheckResult::Wrong(actual))
            }
            Err(_) => Ok(CnameCheckResult::Missing),
        }
    }

    // ========================================================================
    // Domain name validation
    // ========================================================================

    /// Validate a domain name (basic checks).
    pub fn validate_domain(domain: &str) -> Result<(), DomainError> {
        let domain = domain.trim().to_lowercase();

        if domain.is_empty() {
            return Err(DomainError::InvalidDomain);
        }

        // Must contain at least one dot
        if !domain.contains('.') {
            return Err(DomainError::InvalidDomain);
        }

        // No protocol prefix
        if domain.starts_with("http://") || domain.starts_with("https://") {
            return Err(DomainError::InvalidDomain);
        }

        // No trailing dot (except root, which we don't allow)
        if domain.ends_with('.') {
            return Err(DomainError::InvalidDomain);
        }

        // Each label must be 1-63 chars, total max 253
        if domain.len() > 253 {
            return Err(DomainError::InvalidDomain);
        }

        for label in domain.split('.') {
            if label.is_empty() || label.len() > 63 {
                return Err(DomainError::InvalidDomain);
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(DomainError::InvalidDomain);
            }
            if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Err(DomainError::InvalidDomain);
            }
        }

        Ok(())
    }

    // ========================================================================
    // Database operations
    // ========================================================================

    /// Create a new domain record.
    ///
    /// Returns `PendingDeletion` if the domain was recently removed and
    /// is still being cleaned up (soft-deleted but not yet hard-deleted).
    pub async fn create(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        domain_name: &str,
    ) -> Result<Domain, DomainError> {
        Self::validate_domain(domain_name)?;
        let domain_name = domain_name.trim().to_lowercase();

        // Check for a soft-deleted domain with the same name still awaiting cleanup
        if Self::is_pending_deletion(db, &domain_name).await? {
            return Err(DomainError::PendingDeletion);
        }

        let domain_id = Uuid::now_v7();

        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::insert_sqlite(db, &domain_id, env_id, &domain_name).await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::insert_postgres(db, &domain_id, env_id, &domain_name).await?;
            }
        }

        Ok(Domain {
            domain_id,
            env_id: *env_id,
            domain: domain_name,
            verified_at: None,
            tls_provisioned_at: None,
            created_at: Utc::now(),
            certificate_ref: None,
            validation_cname_name: None,
            validation_cname_value: None,
            routing_ref: None,
            routing_domain: None,
            deleted_at: None,
            provisioning_error: None,
        })
    }

    async fn insert_sqlite(
        db: &Pool<Sqlite>,
        domain_id: &Uuid,
        env_id: &Uuid,
        domain: &str,
    ) -> Result<(), DomainError> {
        sqlx::query(
            r#"
            INSERT INTO domain (domain_id, env_id, domain)
            VALUES (?, ?, ?)
            "#,
        )
        .bind(domain_id)
        .bind(env_id)
        .bind(domain)
        .execute(db)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if db_err.message().contains("UNIQUE") => {
                DomainError::AlreadyExists
            }
            _ => DomainError::Database(e),
        })?;

        Ok(())
    }

    async fn insert_postgres(
        db: &Pool<Postgres>,
        domain_id: &Uuid,
        env_id: &Uuid,
        domain: &str,
    ) -> Result<(), DomainError> {
        sqlx::query(
            r#"
            INSERT INTO domain (domain_id, env_id, domain)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(domain_id)
        .bind(env_id)
        .bind(domain)
        .execute(db)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) => {
                if let Some(code) = db_err.code()
                    && code == "23505"
                {
                    return DomainError::AlreadyExists;
                }
                DomainError::Database(e)
            }
            _ => DomainError::Database(e),
        })?;

        Ok(())
    }

    /// Get a domain by ID.
    pub async fn get_domain(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
    ) -> Result<Domain, DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::get_domain_sqlite(db, domain_id).await,
            crate::db::DatabasePool::Postgres(db) => Self::get_domain_postgres(db, domain_id).await,
        }
    }

    async fn get_domain_sqlite(db: &Pool<Sqlite>, domain_id: &Uuid) -> Result<Domain, DomainError> {
        let query = format!("SELECT {} FROM domain WHERE domain_id = ?", DOMAIN_COLUMNS);
        sqlx::query_as::<_, Domain>(&query)
            .bind(domain_id)
            .fetch_one(db)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => DomainError::NotFound,
                other => DomainError::Database(other),
            })
    }

    async fn get_domain_postgres(
        db: &Pool<Postgres>,
        domain_id: &Uuid,
    ) -> Result<Domain, DomainError> {
        let query = format!("SELECT {} FROM domain WHERE domain_id = $1", DOMAIN_COLUMNS);
        sqlx::query_as::<_, Domain>(&query)
            .bind(domain_id)
            .fetch_one(db)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => DomainError::NotFound,
                other => DomainError::Database(other),
            })
    }

    /// Look up a domain by its domain name (for request routing).
    /// This is the hot path used by the domain resolution middleware.
    pub async fn get_by_domain_name(
        db: &crate::db::DatabasePool,
        domain_name: &str,
    ) -> Result<Domain, DomainError> {
        let domain_name = domain_name.trim().to_lowercase();
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_by_domain_name_sqlite(db, &domain_name).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_by_domain_name_postgres(db, &domain_name).await
            }
        }
    }

    async fn get_by_domain_name_sqlite(
        db: &Pool<Sqlite>,
        domain_name: &str,
    ) -> Result<Domain, DomainError> {
        let query = format!(
            "SELECT {} FROM domain WHERE domain = ? AND verified_at IS NOT NULL AND deleted_at IS NULL",
            DOMAIN_COLUMNS
        );
        sqlx::query_as::<_, Domain>(&query)
            .bind(domain_name)
            .fetch_one(db)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => DomainError::NotFound,
                other => DomainError::Database(other),
            })
    }

    async fn get_by_domain_name_postgres(
        db: &Pool<Postgres>,
        domain_name: &str,
    ) -> Result<Domain, DomainError> {
        let query = format!(
            "SELECT {} FROM domain WHERE domain = $1 AND verified_at IS NOT NULL AND deleted_at IS NULL",
            DOMAIN_COLUMNS
        );
        sqlx::query_as::<_, Domain>(&query)
            .bind(domain_name)
            .fetch_one(db)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => DomainError::NotFound,
                other => DomainError::Database(other),
            })
    }

    /// List all domains for an environment.
    pub async fn list_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<Domain>, DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::list_by_env_sqlite(db, env_id).await,
            crate::db::DatabasePool::Postgres(db) => Self::list_by_env_postgres(db, env_id).await,
        }
    }

    async fn list_by_env_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
    ) -> Result<Vec<Domain>, DomainError> {
        let query = format!(
            "SELECT {} FROM domain WHERE env_id = ? ORDER BY created_at DESC",
            DOMAIN_COLUMNS
        );
        Ok(sqlx::query_as::<_, Domain>(&query)
            .bind(env_id)
            .fetch_all(db)
            .await?)
    }

    async fn list_by_env_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
    ) -> Result<Vec<Domain>, DomainError> {
        let query = format!(
            "SELECT {} FROM domain WHERE env_id = $1 ORDER BY created_at DESC",
            DOMAIN_COLUMNS
        );
        Ok(sqlx::query_as::<_, Domain>(&query)
            .bind(env_id)
            .fetch_all(db)
            .await?)
    }

    /// List all unverified domains (for background verification worker).
    pub async fn list_unverified(db: &crate::db::DatabasePool) -> Result<Vec<Domain>, DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::list_unverified_sqlite(db).await,
            crate::db::DatabasePool::Postgres(db) => Self::list_unverified_postgres(db).await,
        }
    }

    async fn list_unverified_sqlite(db: &Pool<Sqlite>) -> Result<Vec<Domain>, DomainError> {
        let query = format!(
            "SELECT {} FROM domain WHERE verified_at IS NULL AND deleted_at IS NULL ORDER BY created_at ASC",
            DOMAIN_COLUMNS
        );
        Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
    }

    async fn list_unverified_postgres(db: &Pool<Postgres>) -> Result<Vec<Domain>, DomainError> {
        let query = format!(
            "SELECT {} FROM domain WHERE verified_at IS NULL AND deleted_at IS NULL ORDER BY created_at ASC",
            DOMAIN_COLUMNS
        );
        Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
    }

    /// List domains that don't yet have a certificate (need initial provisioning).
    pub async fn list_pending_certificate(
        db: &crate::db::DatabasePool,
    ) -> Result<Vec<Domain>, DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let query = format!(
                    "SELECT {} FROM domain WHERE certificate_ref IS NULL AND deleted_at IS NULL ORDER BY created_at ASC",
                    DOMAIN_COLUMNS
                );
                Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let query = format!(
                    "SELECT {} FROM domain WHERE certificate_ref IS NULL AND deleted_at IS NULL ORDER BY created_at ASC",
                    DOMAIN_COLUMNS
                );
                Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
            }
        }
    }

    /// List domains that are verified but don't yet have a routing target.
    pub async fn list_pending_routing(
        db: &crate::db::DatabasePool,
    ) -> Result<Vec<Domain>, DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let query = format!(
                    "SELECT {} FROM domain WHERE verified_at IS NOT NULL AND routing_ref IS NULL AND deleted_at IS NULL ORDER BY created_at ASC",
                    DOMAIN_COLUMNS
                );
                Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let query = format!(
                    "SELECT {} FROM domain WHERE verified_at IS NOT NULL AND routing_ref IS NULL AND deleted_at IS NULL ORDER BY created_at ASC",
                    DOMAIN_COLUMNS
                );
                Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
            }
        }
    }

    /// List domains that have a CF distribution but TLS is not yet provisioned (still deploying).
    pub async fn list_deploying(db: &crate::db::DatabasePool) -> Result<Vec<Domain>, DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let query = format!(
                    "SELECT {} FROM domain WHERE routing_ref IS NOT NULL AND tls_provisioned_at IS NULL AND deleted_at IS NULL ORDER BY created_at ASC",
                    DOMAIN_COLUMNS
                );
                Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let query = format!(
                    "SELECT {} FROM domain WHERE routing_ref IS NOT NULL AND tls_provisioned_at IS NULL AND deleted_at IS NULL ORDER BY created_at ASC",
                    DOMAIN_COLUMNS
                );
                Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
            }
        }
    }

    // ========================================================================
    // Counting (for limit enforcement)
    // ========================================================================

    /// Count the total number of custom domains across all environments in an org.
    pub async fn count_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<i64, DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM domain d JOIN env e ON d.env_id = e.env_id WHERE e.org_id = ? AND d.deleted_at IS NULL",
                )
                .bind(org_id)
                .fetch_one(db)
                .await?;
                Ok(row.0)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM domain d JOIN env e ON d.env_id = e.env_id WHERE e.org_id = $1 AND d.deleted_at IS NULL",
                )
                .bind(org_id)
                .fetch_one(db)
                .await?;
                Ok(row.0)
            }
        }
    }

    // ========================================================================
    // Update operations
    // ========================================================================

    /// Mark domain as verified.
    pub async fn mark_verified(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
    ) -> Result<(), DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::mark_verified_sqlite(db, domain_id).await,
            crate::db::DatabasePool::Postgres(db) => {
                Self::mark_verified_postgres(db, domain_id).await
            }
        }
    }

    async fn mark_verified_sqlite(db: &Pool<Sqlite>, domain_id: &Uuid) -> Result<(), DomainError> {
        sqlx::query("UPDATE domain SET verified_at = datetime('now') WHERE domain_id = ?")
            .bind(domain_id)
            .execute(db)
            .await?;
        Ok(())
    }

    async fn mark_verified_postgres(
        db: &Pool<Postgres>,
        domain_id: &Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query("UPDATE domain SET verified_at = now() WHERE domain_id = $1")
            .bind(domain_id)
            .execute(db)
            .await?;
        Ok(())
    }

    /// Mark domain as TLS/routing provisioned.
    pub async fn mark_tls_provisioned(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
    ) -> Result<(), DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::mark_tls_provisioned_sqlite(db, domain_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::mark_tls_provisioned_postgres(db, domain_id).await
            }
        }
    }

    async fn mark_tls_provisioned_sqlite(
        db: &Pool<Sqlite>,
        domain_id: &Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query("UPDATE domain SET tls_provisioned_at = datetime('now') WHERE domain_id = ?")
            .bind(domain_id)
            .execute(db)
            .await?;
        Ok(())
    }

    async fn mark_tls_provisioned_postgres(
        db: &Pool<Postgres>,
        domain_id: &Uuid,
    ) -> Result<(), DomainError> {
        sqlx::query("UPDATE domain SET tls_provisioned_at = now() WHERE domain_id = $1")
            .bind(domain_id)
            .execute(db)
            .await?;
        Ok(())
    }

    /// Store the provider certificate reference and validation CNAME records.
    pub async fn set_certificate_data(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
        certificate_arn: &str,
        cname_name: &str,
        cname_value: &str,
    ) -> Result<(), DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query(
                    "UPDATE domain SET certificate_ref = ?, validation_cname_name = ?, validation_cname_value = ? WHERE domain_id = ?",
                )
                .bind(certificate_arn)
                .bind(cname_name)
                .bind(cname_value)
                .bind(domain_id)
                .execute(db)
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query(
                    "UPDATE domain SET certificate_ref = $1, validation_cname_name = $2, validation_cname_value = $3 WHERE domain_id = $4",
                )
                .bind(certificate_arn)
                .bind(cname_name)
                .bind(cname_value)
                .bind(domain_id)
                .execute(db)
                .await?;
            }
        }
        Ok(())
    }

    /// Store provider routing target data after creation.
    pub async fn set_routing_target(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
        distribution_id: &str,
        distribution_domain: &str,
    ) -> Result<(), DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query(
                    "UPDATE domain SET routing_ref = ?, routing_domain = ? WHERE domain_id = ?",
                )
                .bind(distribution_id)
                .bind(distribution_domain)
                .bind(domain_id)
                .execute(db)
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query(
                    "UPDATE domain SET routing_ref = $1, routing_domain = $2 WHERE domain_id = $3",
                )
                .bind(distribution_id)
                .bind(distribution_domain)
                .bind(domain_id)
                .execute(db)
                .await?;
            }
        }
        Ok(())
    }

    /// Store a user-facing provisioning error (e.g., CNAME conflict with another distribution).
    pub async fn set_provisioning_error(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
        error: &str,
    ) -> Result<(), DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query("UPDATE domain SET provisioning_error = ? WHERE domain_id = ?")
                    .bind(error)
                    .bind(domain_id)
                    .execute(db)
                    .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query("UPDATE domain SET provisioning_error = $1 WHERE domain_id = $2")
                    .bind(error)
                    .bind(domain_id)
                    .execute(db)
                    .await?;
            }
        }
        Ok(())
    }

    /// Clear the provisioning error (called on successful CF creation or user retry).
    pub async fn clear_provisioning_error(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
    ) -> Result<(), DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query("UPDATE domain SET provisioning_error = NULL WHERE domain_id = ?")
                    .bind(domain_id)
                    .execute(db)
                    .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query("UPDATE domain SET provisioning_error = NULL WHERE domain_id = $1")
                    .bind(domain_id)
                    .execute(db)
                    .await?;
            }
        }
        Ok(())
    }

    /// Soft-delete a domain (mark for deletion; worker will clean up provider resources).
    pub async fn soft_delete(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
    ) -> Result<(), DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query("UPDATE domain SET deleted_at = datetime('now') WHERE domain_id = ?")
                    .bind(domain_id)
                    .execute(db)
                    .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query("UPDATE domain SET deleted_at = now() WHERE domain_id = $1")
                    .bind(domain_id)
                    .execute(db)
                    .await?;
            }
        }
        Ok(())
    }

    /// Permanently remove a domain record (called by the worker after provider cleanup).
    pub async fn hard_delete(
        db: &crate::db::DatabasePool,
        domain_id: &Uuid,
    ) -> Result<(), DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query("DELETE FROM domain WHERE domain_id = ?")
                    .bind(domain_id)
                    .execute(db)
                    .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query("DELETE FROM domain WHERE domain_id = $1")
                    .bind(domain_id)
                    .execute(db)
                    .await?;
            }
        }
        Ok(())
    }

    /// Check if a domain name has a soft-deleted record still awaiting cleanup.
    async fn is_pending_deletion(
        db: &crate::db::DatabasePool,
        domain_name: &str,
    ) -> Result<bool, DomainError> {
        let count: i64 = match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM domain WHERE domain = ? AND deleted_at IS NOT NULL",
                )
                .bind(domain_name)
                .fetch_one(db)
                .await?
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM domain WHERE domain = $1 AND deleted_at IS NOT NULL",
                )
                .bind(domain_name)
                .fetch_one(db)
                .await?
            }
        };
        Ok(count > 0)
    }

    /// List domains that have been soft-deleted and need provider resource cleanup.
    pub async fn list_pending_deletion(
        db: &crate::db::DatabasePool,
    ) -> Result<Vec<Domain>, DomainError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let query = format!(
                    "SELECT {} FROM domain WHERE deleted_at IS NOT NULL ORDER BY deleted_at ASC",
                    DOMAIN_COLUMNS
                );
                Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let query = format!(
                    "SELECT {} FROM domain WHERE deleted_at IS NOT NULL ORDER BY deleted_at ASC",
                    DOMAIN_COLUMNS
                );
                Ok(sqlx::query_as::<_, Domain>(&query).fetch_all(db).await?)
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_validation_valid() {
        assert!(Domain::validate_domain("mcp.example.com").is_ok());
        assert!(Domain::validate_domain("webhook.example.io").is_ok());
        assert!(Domain::validate_domain("api.my-company.com").is_ok());
        assert!(Domain::validate_domain("sub.domain.example.com").is_ok());
        assert!(Domain::validate_domain("a.co").is_ok());
        assert!(Domain::validate_domain("  MCP.ACME.COM  ").is_ok());
    }

    #[test]
    fn test_domain_validation_invalid() {
        assert!(Domain::validate_domain("").is_err());
        assert!(Domain::validate_domain("localhost").is_err());
        assert!(Domain::validate_domain("https://mcp.example.com").is_err());
        assert!(Domain::validate_domain("http://mcp.example.com").is_err());
        assert!(Domain::validate_domain("mcp.example.com.").is_err());
        assert!(Domain::validate_domain("-mcp.example.com").is_err());
        assert!(Domain::validate_domain("mcp-.example.com").is_err());
        assert!(Domain::validate_domain("mcp..example.com").is_err());
    }

    fn make_domain(verified: bool, cf: bool, tls: bool) -> Domain {
        let now = Utc::now();
        Domain {
            domain_id: Uuid::now_v7(),
            env_id: Uuid::now_v7(),
            domain: "mcp.example.com".to_string(),
            verified_at: if verified { Some(now) } else { None },
            tls_provisioned_at: if tls { Some(now) } else { None },
            created_at: now,
            certificate_ref: Some("arn:aws:acm:us-east-1:123:cert/abc".to_string()),
            validation_cname_name: None,
            validation_cname_value: None,
            routing_ref: if cf {
                Some("E1A2B3C4D5".to_string())
            } else {
                None
            },
            routing_domain: if cf {
                Some("d1234.cloudfront.net".to_string())
            } else {
                None
            },
            deleted_at: None,
            provisioning_error: None,
        }
    }

    #[test]
    fn test_status_pending_validation() {
        let d = make_domain(false, false, false);
        assert_eq!(d.status(), DomainStatus::PendingValidation);
        assert!(!d.is_verified());
        assert!(!d.is_tls_provisioned());
        assert!(!d.is_ready());
    }

    #[test]
    fn test_status_validated() {
        let d = make_domain(true, false, false);
        assert_eq!(d.status(), DomainStatus::Validated);
        assert!(d.is_verified());
        assert!(!d.is_ready());
    }

    #[test]
    fn test_status_provisioning() {
        let d = make_domain(true, true, false);
        assert_eq!(d.status(), DomainStatus::Provisioning);
        assert!(d.has_routing());
        assert!(!d.is_ready());
    }

    #[test]
    fn test_status_active() {
        let d = make_domain(true, true, true);
        assert_eq!(d.status(), DomainStatus::Active);
        assert!(d.is_ready());
    }

    #[test]
    fn test_status_pending_deletion() {
        let mut d = make_domain(true, true, true);
        d.deleted_at = Some(Utc::now());
        assert_eq!(d.status(), DomainStatus::PendingDeletion);
    }
}

use crate::db::{DatabasePool, domain::Domain};
use crate::val::Val;
use async_trait::async_trait;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainCertificateStatus {
    PendingValidation,
    Issued,
    Failed(String),
    Inactive,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainDistributionStatus {
    InProgress,
    Deployed,
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct DomainProviderError {
    pub message: String,
    pub cname_already_exists: bool,
}

impl DomainProviderError {
    pub fn unavailable() -> Self {
        Self {
            message: "Custom domain provisioning is not configured for this build".to_string(),
            cname_already_exists: false,
        }
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cname_already_exists: false,
        }
    }

    pub fn cname_already_exists(domain: impl Into<String>) -> Self {
        Self {
            message: domain.into(),
            cname_already_exists: true,
        }
    }
}

impl std::fmt::Display for DomainProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DomainProviderError {}

#[async_trait]
pub trait DomainProvider: Send + Sync {
    fn is_configured(&self, _conf: &Val) -> bool {
        false
    }

    async fn request_certificate(
        &self,
        _conf: &Val,
        _db: &DatabasePool,
        _domain: &Domain,
    ) -> Result<(), DomainProviderError> {
        Err(DomainProviderError::unavailable())
    }

    async fn certificate_status(
        &self,
        _conf: &Val,
        _domain: &Domain,
        _certificate_ref: &str,
    ) -> Result<DomainCertificateStatus, DomainProviderError> {
        Err(DomainProviderError::unavailable())
    }

    async fn create_distribution(
        &self,
        _conf: &Val,
        _db: &DatabasePool,
        _domain: &Domain,
        _certificate_ref: &str,
    ) -> Result<(), DomainProviderError> {
        Err(DomainProviderError::unavailable())
    }

    async fn distribution_status(
        &self,
        _conf: &Val,
        _distribution_ref: &str,
    ) -> Result<DomainDistributionStatus, DomainProviderError> {
        Err(DomainProviderError::unavailable())
    }

    async fn cleanup_domain(
        &self,
        _conf: &Val,
        _domain: &Domain,
    ) -> Result<(), DomainProviderError> {
        Err(DomainProviderError::unavailable())
    }
}

pub struct NoopDomainProvider;

#[async_trait]
impl DomainProvider for NoopDomainProvider {}

static DOMAIN_PROVIDER: OnceLock<Arc<dyn DomainProvider>> = OnceLock::new();

pub fn set_domain_provider(
    provider: Arc<dyn DomainProvider>,
) -> Result<(), Arc<dyn DomainProvider>> {
    DOMAIN_PROVIDER.set(provider)
}

pub fn domain_provider() -> Arc<dyn DomainProvider> {
    DOMAIN_PROVIDER
        .get_or_init(|| Arc::new(NoopDomainProvider))
        .clone()
}

use axum::http::{HeaderMap, StatusCode};
use hot::db::{DatabasePool, OrgPlan};
use hot::val::Val;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone)]
pub struct BillingProviderError {
    pub status: StatusCode,
    pub message: String,
}

impl BillingProviderError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Billing is not configured for this build".to_string(),
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    pub fn into_status_message(self) -> (StatusCode, String) {
        (self.status, self.message)
    }
}

pub struct BillingCheckoutRequest<'a> {
    pub db: &'a DatabasePool,
    pub conf: &'a Val,
    pub org_id: uuid::Uuid,
    pub org_slug: &'a str,
    pub org_name: &'a str,
    pub user_id: uuid::Uuid,
    pub user_email: &'a str,
    pub plan_id: &'a str,
    pub billing_period: &'a str,
}

pub struct BillingCheckoutSuccessRequest<'a> {
    pub db: &'a DatabasePool,
    pub conf: &'a Val,
    pub session_id: &'a str,
}

pub struct BillingCheckoutSuccess {
    pub org_id: uuid::Uuid,
    pub org_slug: String,
}

pub struct BillingSubscriptionActionRequest<'a> {
    pub db: &'a DatabasePool,
    pub conf: &'a Val,
    pub org_plan: &'a OrgPlan,
}

pub struct BillingWebhookRequest<'a> {
    pub db: &'a DatabasePool,
    pub conf: &'a Val,
    pub headers: &'a HeaderMap,
    pub body: &'a str,
}

#[async_trait::async_trait]
pub trait BillingProvider: Send + Sync {
    fn is_configured(&self, _conf: &Val) -> bool {
        false
    }

    async fn create_checkout(
        &self,
        _request: BillingCheckoutRequest<'_>,
    ) -> Result<String, BillingProviderError> {
        Err(BillingProviderError::unavailable())
    }

    async fn checkout_success(
        &self,
        _request: BillingCheckoutSuccessRequest<'_>,
    ) -> Result<Option<BillingCheckoutSuccess>, BillingProviderError> {
        Ok(None)
    }

    async fn cancel_subscription(
        &self,
        _request: BillingSubscriptionActionRequest<'_>,
    ) -> Result<(), BillingProviderError> {
        Err(BillingProviderError::unavailable())
    }

    async fn reactivate_subscription(
        &self,
        _request: BillingSubscriptionActionRequest<'_>,
    ) -> Result<(), BillingProviderError> {
        Err(BillingProviderError::unavailable())
    }

    async fn handle_webhook(
        &self,
        _request: BillingWebhookRequest<'_>,
    ) -> Result<(), BillingProviderError> {
        Err(BillingProviderError::unavailable())
    }
}

#[derive(Debug)]
pub struct NoopBillingProvider;

#[async_trait::async_trait]
impl BillingProvider for NoopBillingProvider {}

static BILLING_PROVIDER: OnceLock<Arc<dyn BillingProvider>> = OnceLock::new();

pub fn set_billing_provider(
    provider: Arc<dyn BillingProvider>,
) -> Result<(), Arc<dyn BillingProvider>> {
    BILLING_PROVIDER.set(provider)
}

pub fn billing_provider() -> Arc<dyn BillingProvider> {
    BILLING_PROVIDER
        .get_or_init(|| Arc::new(NoopBillingProvider))
        .clone()
}

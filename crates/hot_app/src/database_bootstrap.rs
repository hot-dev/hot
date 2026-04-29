use hot::val::Val;
use std::sync::{Arc, OnceLock};

#[async_trait::async_trait]
pub trait DatabaseBootstrap: Send + Sync {
    async fn bootstrap(&self, _conf: &Val) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct NoopDatabaseBootstrap;

#[async_trait::async_trait]
impl DatabaseBootstrap for NoopDatabaseBootstrap {}

static DATABASE_BOOTSTRAP: OnceLock<Arc<dyn DatabaseBootstrap>> = OnceLock::new();

pub fn set_database_bootstrap(
    bootstrap: Arc<dyn DatabaseBootstrap>,
) -> Result<(), Arc<dyn DatabaseBootstrap>> {
    DATABASE_BOOTSTRAP.set(bootstrap)
}

pub fn database_bootstrap() -> Arc<dyn DatabaseBootstrap> {
    DATABASE_BOOTSTRAP
        .get_or_init(|| Arc::new(NoopDatabaseBootstrap))
        .clone()
}

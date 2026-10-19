//! Deployment validation for the runtime login. This uses a raw pool only
//! before it is wrapped as a scoped executor; it does not expose that pool.

use sqlx::PgPool;

pub async fn validate_runtime_role(pool: &PgPool, schema: &str) -> Result<(), sqlx::Error> {
    let privileged: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_roles r WHERE (r.rolsuper OR r.rolbypassrls) AND (r.rolname = current_user OR pg_has_role(current_user, r.oid, 'MEMBER')))"
    ).fetch_one(pool).await?;
    if privileged {
        return Err(sqlx::Error::Protocol("RLS runtime login must not be superuser, BYPASSRLS, or a member of a privileged role".into()));
    }
    let owns_tables: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relkind IN ('r','p') AND pg_has_role(current_user, c.relowner, 'MEMBER'))"
    ).bind(schema).fetch_one(pool).await?;
    if owns_tables {
        return Err(sqlx::Error::Protocol("RLS runtime login must not own or be able to assume ownership of application tables".into()));
    }
    let can_truncate: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relkind IN ('r','p') AND has_table_privilege(current_user, c.oid, 'TRUNCATE'))"
    ).bind(schema).fetch_one(pool).await?;
    if can_truncate {
        return Err(sqlx::Error::Protocol("RLS runtime login must not have TRUNCATE privileges".into()));
    }
    let uncovered: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1 AND c.relkind IN ('r','p')
           AND c.relname NOT IN ('_sqlx_migrations','build_type','email_verification_status','invite_status','org_user_role','run_status','run_type','task_status','team_user_role','org_plan_status','alert_delivery_status','alert_destination_type','plan')
           AND (NOT c.relrowsecurity OR NOT c.relforcerowsecurity)
         ORDER BY c.relname"
    ).bind(schema).fetch_all(pool).await?;
    if !uncovered.is_empty() {
        return Err(sqlx::Error::Protocol(format!("RLS coverage is incomplete for: {}", uncovered.join(", "))));
    }
    let helper_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1 AND p.proname = 'scope_env_allows')"
    ).bind(schema).fetch_one(pool).await?;
    if !helper_exists {
        return Err(sqlx::Error::Protocol("RLS migrations must be applied before starting runtime services".into()));
    }
    Ok(())
}

/// Provision per-store HNSW indexes using separately configured migration
/// credentials. Runtime ensure_store never performs DDL. This administrative
/// path exposes no pool and needs no runtime BYPASSRLS privilege.
pub async fn provision_store_indexes(conf: &crate::val::Val) -> Result<usize, String> {
    use sqlx::Row;
    use super::{DatabaseScope, MaintenanceOperation};
    let uri = conf.get_str_or_default("db.migration-uri", &conf.get_str_or_default("migration-uri", ""));
    if uri.is_empty() || !matches!(super::DatabaseType::from_uri(&uri), Ok(super::DatabaseType::Postgres)) {
        return Err("Store index provisioning requires a PostgreSQL db.migration-uri".into());
    }
    let schema = conf.get_str_or_default("db.schema", &conf.get_str_or_default("schema", "hot"));
    if schema != "hot" {
        return Err("PostgreSQL RLS currently supports only the hot schema".into());
    }
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&uri)
        .await.map_err(|e| format!("Cannot connect index provisioner: {e}"))?;
    let result = async {
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        DatabaseScope::Maintenance { operation: MaintenanceOperation::StoreProvisioning, org_id: None }
            .apply_to_connection(&mut tx).await.map_err(|e| e.to_string())?;
        let stores = sqlx::query("SELECT org_id, env_id, name, quote_literal(name) AS quoted_name, embedding_conf FROM hot.store_map WHERE embedding_conf IS NOT NULL")
            .fetch_all(&mut *tx).await.map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        let mut count = 0;
        for store in stores {
            let org_id: uuid::Uuid = store.try_get("org_id").map_err(|e| e.to_string())?;
            let env_id: uuid::Uuid = store.try_get("env_id").map_err(|e| e.to_string())?;
            let name: String = store.try_get("name").map_err(|e| e.to_string())?;
            let config: serde_json::Value = store.try_get("embedding_conf").map_err(|e| e.to_string())?;
            let dimensions = config.get("dimensions").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| format!("Store {name:?} has no positive embedding dimensions"))?;
            if !(1..=2000).contains(&dimensions) {
                return Err(format!("Store {name:?} uses {dimensions} dimensions; vector HNSW supports 1 through 2000"));
            }
            let identity = format!("{org_id}/{env_id}/{name}/{dimensions}");
            let digest = blake3::hash(identity.as_bytes()).to_hex();
            let index_name = format!("store_hnsw_{}", &digest.as_str()[..32]);
            let valid: Option<bool> = sqlx::query_scalar("SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'hot' AND c.relname = $1")
                .bind(&index_name).fetch_optional(&pool).await.map_err(|e| e.to_string())?;
            if valid == Some(true) { continue; }
            if valid == Some(false) {
                return Err(format!("Index hot.{index_name} is invalid after an interrupted build; drop that invalid index and rerun provisioning"));
            }
            // PostgreSQL quotes the arbitrary literal; UUID/dimensions are
            // typed and the identifier contains a fixed prefix + hex hash.
            let literal: String = store.try_get("quoted_name").map_err(|e| e.to_string())?;
            let statement = format!("CREATE INDEX CONCURRENTLY {index_name} ON hot.store_map_entry USING hnsw ((embedding::hot.vector({dimensions})) hot.vector_cosine_ops) WHERE org_id = '{org_id}'::uuid AND env_id = '{env_id}'::uuid AND store_name = {literal} AND embedding IS NOT NULL AND hot.vector_dims(embedding) = {dimensions}");
            sqlx::query(sqlx::AssertSqlSafe(statement.as_str())).execute(&pool).await.map_err(|e| format!("Cannot provision store {name:?}: {e}"))?;
            count += 1;
        }
        Ok(count)
    }.await;
    pool.close().await;
    result
}

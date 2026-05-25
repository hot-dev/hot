pub mod embedding;
pub mod postgres;
pub mod sqlite;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::val::Val;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreMapConfig {
    pub name: String,
    pub embedding_model: Option<String>,
    pub embedding_field: Option<String>,
    pub embedding_dimensions: Option<u32>,
    pub text_search: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreEntry {
    pub key: serde_json::Value,
    pub value: serde_json::Value,
    pub seq: i64,
    pub embedding: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl StoreEntry {
    pub fn to_info(&self) -> StoreEntryInfo {
        StoreEntryInfo {
            key: self.key.clone(),
            seq: self.seq,
            embedding_dimensions: self.embedding.as_ref().map(|e| e.len()),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreEntryInfo {
    pub key: serde_json::Value,
    pub seq: i64,
    pub embedding_dimensions: Option<usize>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Summary of a named store map within the current `(org_id, env_id)` scope.
/// Returned by [`Store::list_maps`] for store browsing and admin UIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreMapInfo {
    pub name: String,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: Option<u32>,
    pub embedding_field: Option<String>,
    pub text_search: bool,
    pub entry_count: i64,
    pub storage_bytes: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub entry: StoreEntry,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchInfoPage {
    pub entries: Vec<StoreEntryInfo>,
    pub total_entries: usize,
}

#[derive(Debug, Clone, Default)]
pub enum SearchMode {
    #[default]
    Semantic,
    Keyword,
    Hybrid,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: Option<usize>,
    pub min_score: Option<f64>,
    pub mode: SearchMode,
}

#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[async_trait]
pub trait Store: Send + Sync {
    /// Ensure the named store exists, creating it if necessary.
    async fn ensure_store(&self, config: &StoreMapConfig) -> Result<(), String>;

    /// Write a key-value pair. If the key already exists, update in place (preserve seq).
    async fn put(
        &self,
        store_name: &str,
        key: serde_json::Value,
        value: serde_json::Value,
        embedding: Option<Vec<f32>>,
        text_content: Option<String>,
    ) -> Result<StoreEntry, String>;

    /// Get a value by key. Returns None if not found.
    async fn get(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntry>, String>;

    /// Get metadata for an entry without selecting its value.
    async fn get_info(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntryInfo>, String>;

    /// Delete by key. Returns true if the key existed.
    async fn delete(&self, store_name: &str, key: &serde_json::Value) -> Result<bool, String>;

    /// All keys in insertion order.
    async fn keys(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String>;

    /// All values in insertion order.
    async fn vals(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String>;

    /// Number of entries.
    async fn length(&self, store_name: &str) -> Result<usize, String>;

    /// First entry by insertion order.
    async fn first(&self, store_name: &str) -> Result<Option<StoreEntry>, String>;

    /// Last entry by insertion order.
    async fn last(&self, store_name: &str) -> Result<Option<StoreEntry>, String>;

    /// Paginated listing in insertion order.
    async fn list(&self, store_name: &str, options: ListOptions)
    -> Result<Vec<StoreEntry>, String>;

    /// Paginated metadata listing in insertion order, without selecting values.
    async fn list_info(
        &self,
        store_name: &str,
        options: ListOptions,
    ) -> Result<Vec<StoreEntryInfo>, String>;

    /// Remove all entries from a store (but keep the store itself).
    async fn clear(&self, store_name: &str) -> Result<(), String>;

    /// Delete the store entirely (schema + data).
    async fn destroy(&self, store_name: &str) -> Result<(), String>;

    /// Batch write from a set of key-value pairs.
    async fn put_many(
        &self,
        store_name: &str,
        entries: Vec<(
            serde_json::Value,
            serde_json::Value,
            Option<Vec<f32>>,
            Option<String>,
        )>,
    ) -> Result<usize, String>;

    /// Semantic / keyword / hybrid search. `query_embedding` is pre-computed by the caller.
    async fn search(
        &self,
        store_name: &str,
        query_text: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        options: SearchOptions,
    ) -> Result<Vec<SearchResult>, String>;

    /// Metadata-only search for browser UIs. Implementations should search keys
    /// and values by keyword, and may additionally include semantic matches when
    /// `query_embedding` is present.
    async fn search_info(
        &self,
        store_name: &str,
        query_text: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        search_options: SearchOptions,
        list_options: ListOptions,
    ) -> Result<SearchInfoPage, String>;

    /// Total bytes used by all entries across all stores for this backend instance.
    async fn storage_bytes(&self) -> Result<i64, String>;

    fn storage_type(&self) -> &str;

    /// Enumerate all named stores in the current `(org_id, env_id)` scope.
    /// Returned entries include cheap aggregates (entry count, total bytes) so
    /// admin UIs can render summary tables without an extra round-trip.
    async fn list_maps(&self) -> Result<Vec<StoreMapInfo>, String>;
}

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
pub enum StoreBackendType {
    #[default]
    Sqlite,
    Postgres,
}

impl StoreBackendType {
    fn parse(raw: &str) -> Self {
        match raw.to_lowercase().as_str() {
            "postgres" | "pg" => StoreBackendType::Postgres,
            _ => StoreBackendType::Sqlite,
        }
    }

    pub fn from_config(conf: &Val) -> Self {
        let env_store_type = std::env::var("HOT_STORE_TYPE").ok();
        Self::from_config_with_env(conf, env_store_type.as_deref())
    }

    fn from_config_with_env(conf: &Val, env_store_type: Option<&str>) -> Self {
        if let Some(store_type) = env_store_type {
            return Self::parse(store_type);
        }
        conf.get("store.type")
            .and_then(|v| match v {
                Val::Str(s) => Some(Self::parse(s.as_ref())),
                _ => None,
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

pub async fn store_from_config_with_db(
    conf: &Val,
    db_pool: Option<Arc<crate::db::DatabasePool>>,
    org_id: Option<uuid::Uuid>,
    env_id: Option<uuid::Uuid>,
) -> Result<Box<dyn Store>, String> {
    let store_type = StoreBackendType::from_config(conf);
    let eid = env_id.ok_or_else(|| {
        "::hot::store requires an env_id because stores are environment-scoped.".to_string()
    })?;

    match store_type {
        StoreBackendType::Sqlite => {
            let pool = db_pool.ok_or_else(|| {
                "SQLite store requires a DatabasePool. Use store_from_config_with_db() with a pool."
                    .to_string()
            })?;
            let oid = org_id.ok_or_else(|| {
                "SQLite store requires an org_id. Use store_from_config_with_db() with the active org."
                    .to_string()
            })?;

            // One-shot legacy import: copy rows from any pre-2.0 single-tenant
            // .hot/store/store.db into the main hot DB on first run. No-op if the
            // legacy file is absent or already imported.
            if let Err(e) = sqlite::maybe_import_legacy_store(&pool, ".hot/store", oid, eid).await {
                tracing::warn!("Legacy ::hot::store import skipped: {e}");
            }

            let store = sqlite::SqliteStore::new(pool, oid, eid);
            Ok(Box::new(store))
        }
        StoreBackendType::Postgres => {
            let pool = db_pool.ok_or_else(|| {
                "Postgres store requires a DatabasePool. Use store_from_config_with_db() with a pool.".to_string()
            })?;
            let oid = org_id.ok_or_else(|| "Postgres store requires an org_id".to_string())?;
            let store = postgres::PgStore::new(pool, oid, eid);
            Ok(Box::new(store))
        }
    }
}

/// Extract a StoreMapConfig from a Hot `::hot::store/Map` Val.
pub fn store_map_config_from_val(val: &Val) -> Result<StoreMapConfig, String> {
    let inner = match val {
        Val::Map(m) => {
            if let Some(Val::Map(inner_val)) = m.get(&Val::from("$val")) {
                inner_val.clone()
            } else {
                return Err("Expected a ::hot::store/Map typed value".to_string());
            }
        }
        _ => return Err("Expected a ::hot::store/Map typed value".to_string()),
    };

    let name = match inner.get(&Val::from("name")) {
        Some(Val::Str(s)) => s.to_string(),
        _ => return Err("::hot::store/Map requires a 'name' field (Str)".to_string()),
    };

    let embedding_field_val = inner.get(&Val::from("embedding"));

    let (embedding_model, embedding_field, embedding_dimensions, text_search) =
        match embedding_field_val {
            // embedding: true  → use system defaults (resolved later)
            Some(Val::Bool(true)) => (Some("__system_default__".to_string()), None, None, false),
            // embedding: null / false / missing → off
            Some(Val::Bool(false)) | Some(Val::Null) | None => (None, None, None, false),
            // embedding: Embedding({...}) → extract fields
            Some(Val::Map(em)) => {
                let em_inner = if let Some(Val::Map(iv)) = em.get(&Val::from("$val")) {
                    iv
                } else {
                    em
                };
                let model = em_inner
                    .get(&Val::from("model"))
                    .and_then(|v| match v {
                        Val::Str(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .or(Some("__system_default__".to_string()));
                let field = em_inner.get(&Val::from("field")).and_then(|v| match v {
                    Val::Str(s) => Some(s.to_string()),
                    _ => None,
                });
                let ts = em_inner
                    .get(&Val::from("text-search"))
                    .and_then(|v| match v {
                        Val::Bool(b) => Some(*b),
                        _ => None,
                    })
                    .unwrap_or(false);
                (model, field, None, ts)
            }
            _ => (None, None, None, false),
        };

    Ok(StoreMapConfig {
        name,
        embedding_model,
        embedding_field,
        embedding_dimensions,
        text_search,
    })
}

/// Resolve `__system_default__` model placeholder using hot.hot config.
pub fn resolve_embedding_model(config: &mut StoreMapConfig, conf: Option<&Val>) {
    if config.embedding_model.as_deref() == Some("__system_default__") {
        let resolved = conf
            .and_then(|c| c.get("store.embedding.model"))
            .and_then(|v| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "bge-base-en-v1.5".to_string());
        config.embedding_model = Some(resolved);
    }

    if config.embedding_field.is_none() && config.embedding_model.is_some() {
        let resolved = conf
            .and_then(|c| c.get("store.embedding.field"))
            .and_then(|v| match v {
                Val::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "content".to_string());
        config.embedding_field = Some(resolved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_val(name: &str) -> Val {
        let mut inner = indexmap::IndexMap::new();
        inner.insert(Val::from("name"), Val::from(name));
        let mut outer = indexmap::IndexMap::new();
        outer.insert(Val::from("$type"), Val::from("::hot::store/Map"));
        outer.insert(Val::from("$val"), Val::Map(Box::new(inner)));
        Val::Map(Box::new(outer))
    }

    fn map_val_with_embedding(name: &str, embedding: Val) -> Val {
        let mut inner = indexmap::IndexMap::new();
        inner.insert(Val::from("name"), Val::from(name));
        inner.insert(Val::from("embedding"), embedding);
        let mut outer = indexmap::IndexMap::new();
        outer.insert(Val::from("$type"), Val::from("::hot::store/Map"));
        outer.insert(Val::from("$val"), Val::Map(Box::new(inner)));
        Val::Map(Box::new(outer))
    }

    #[test]
    fn test_store_backend_type_from_config() {
        let conf = crate::val!({
            "store": {
                "type": "postgres",
            },
        });
        assert_eq!(
            StoreBackendType::from_config_with_env(&conf, None),
            StoreBackendType::Postgres
        );
    }

    #[test]
    fn test_store_backend_type_defaults_to_sqlite() {
        assert_eq!(
            StoreBackendType::from_config_with_env(&Val::map_empty(), None),
            StoreBackendType::Sqlite
        );
    }

    #[test]
    fn test_store_backend_type_env_overrides_config() {
        let conf = crate::val!({
            "store": {
                "type": "sqlite",
            },
        });
        assert_eq!(
            StoreBackendType::from_config_with_env(&conf, Some("pg")),
            StoreBackendType::Postgres
        );
    }

    #[test]
    fn test_config_from_val_plain() {
        let val = map_val("settings");
        let config = store_map_config_from_val(&val).unwrap();
        assert_eq!(config.name, "settings");
        assert!(config.embedding_model.is_none());
        assert!(config.embedding_field.is_none());
        assert!(!config.text_search);
    }

    #[test]
    fn test_config_from_val_embedding_true() {
        let val = map_val_with_embedding("kb", Val::Bool(true));
        let config = store_map_config_from_val(&val).unwrap();
        assert_eq!(config.name, "kb");
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("__system_default__")
        );
    }

    #[test]
    fn test_config_from_val_embedding_false() {
        let val = map_val_with_embedding("kb", Val::Bool(false));
        let config = store_map_config_from_val(&val).unwrap();
        assert!(config.embedding_model.is_none());
    }

    #[test]
    fn test_config_from_val_embedding_map() {
        let mut em = indexmap::IndexMap::new();
        em.insert(Val::from("model"), Val::from("text-embedding-3-small"));
        em.insert(Val::from("field"), Val::from("body"));
        em.insert(Val::from("text-search"), Val::Bool(true));
        let val = map_val_with_embedding("docs", Val::Map(Box::new(em)));
        let config = store_map_config_from_val(&val).unwrap();
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(config.embedding_field.as_deref(), Some("body"));
        assert!(config.text_search);
    }

    #[test]
    fn test_config_from_val_missing_name() {
        let mut inner = indexmap::IndexMap::new();
        inner.insert(Val::from("not-name"), Val::from("oops"));
        let mut outer = indexmap::IndexMap::new();
        outer.insert(Val::from("$type"), Val::from("::hot::store/Map"));
        outer.insert(Val::from("$val"), Val::Map(Box::new(inner)));
        let val = Val::Map(Box::new(outer));
        assert!(store_map_config_from_val(&val).is_err());
    }

    #[test]
    fn test_config_from_val_wrong_type() {
        assert!(store_map_config_from_val(&Val::from("not a map")).is_err());
    }

    #[test]
    fn test_resolve_embedding_model_system_default() {
        let mut config = StoreMapConfig {
            name: "t".into(),
            embedding_model: Some("__system_default__".into()),
            embedding_field: None,
            embedding_dimensions: None,
            text_search: false,
        };
        resolve_embedding_model(&mut config, None);
        assert_eq!(config.embedding_model.as_deref(), Some("bge-base-en-v1.5"));
        assert_eq!(config.embedding_field.as_deref(), Some("content"));
    }

    #[test]
    fn test_resolve_embedding_model_with_conf() {
        let conf = crate::val!({
            "store": {
                "embedding": {
                    "model": "text-embedding-3-small",
                    "field": "body"
                }
            }
        });
        let mut config = StoreMapConfig {
            name: "t".into(),
            embedding_model: Some("__system_default__".into()),
            embedding_field: None,
            embedding_dimensions: None,
            text_search: false,
        };
        resolve_embedding_model(&mut config, Some(&conf));
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(config.embedding_field.as_deref(), Some("body"));
    }

    #[test]
    fn test_resolve_embedding_model_explicit_not_replaced() {
        let mut config = StoreMapConfig {
            name: "t".into(),
            embedding_model: Some("my-custom-model".into()),
            embedding_field: Some("text".into()),
            embedding_dimensions: None,
            text_search: false,
        };
        resolve_embedding_model(&mut config, None);
        assert_eq!(config.embedding_model.as_deref(), Some("my-custom-model"));
        assert_eq!(config.embedding_field.as_deref(), Some("text"));
    }
}

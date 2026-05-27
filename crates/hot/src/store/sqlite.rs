use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::db::DatabasePool;

use super::{
    ListOptions, SearchInfoPage, SearchMode, SearchOptions, SearchResult, Store, StoreEntry,
    StoreEntryInfo, StoreMapConfig, StoreMapInfo, effective_embedding_conf,
    embedding_conf_dimensions, embedding_conf_field, embedding_conf_model, embedding_conf_provider,
    validate_store_map_compatibility,
};

/// Local SQLite-backed implementation of [`Store`].
///
/// Mirrors [`super::postgres::PgStore`]: a thin wrapper around the shared
/// [`DatabasePool`] that scopes every row by `(org_id, env_id, store_name)`.
/// The schema lives in the main hot SQLite migrations alongside everything
/// else (see `resources/db/sqlite/migrations/002_add_store_tables.sql`),
/// so there is no separate database file or runtime schema setup.
///
pub struct SqliteStore {
    pool: Arc<DatabasePool>,
    org_id: Uuid,
    env_id: Uuid,
    /// Cache of `ensure_store` calls so we don't re-run the upsert per call.
    ensured: Mutex<HashSet<String>>,
}

impl SqliteStore {
    pub fn new(pool: Arc<DatabasePool>, org_id: Uuid, env_id: Uuid) -> Self {
        Self {
            pool,
            org_id,
            env_id,
            ensured: Mutex::new(HashSet::new()),
        }
    }

    fn sqlite_pool(&self) -> Result<&SqlitePool, String> {
        match self.pool.as_ref() {
            DatabasePool::Sqlite(p) => Ok(p),
            _ => Err("SqliteStore requires a SQLite database pool".to_string()),
        }
    }

    fn row_to_entry(row: &sqlx::sqlite::SqliteRow) -> Result<StoreEntry, String> {
        let key_str: String = row.try_get("key").map_err(|e| e.to_string())?;
        let value_str: String = row.try_get("value").map_err(|e| e.to_string())?;
        let seq: i64 = row.try_get("seq").map_err(|e| e.to_string())?;
        let embedding_bytes: Option<Vec<u8>> =
            row.try_get("embedding").map_err(|e| e.to_string())?;
        let created_at: String = row
            .try_get("created_at")
            .unwrap_or_else(|_| Utc::now().to_rfc3339());
        let updated_at: String = row
            .try_get("updated_at")
            .unwrap_or_else(|_| Utc::now().to_rfc3339());

        let key: serde_json::Value =
            serde_json::from_str(&key_str).map_err(|e| format!("Bad key JSON: {e}"))?;
        let value: serde_json::Value =
            serde_json::from_str(&value_str).map_err(|e| format!("Bad value JSON: {e}"))?;

        let embedding = embedding_bytes.map(|bytes| {
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        });

        Ok(StoreEntry {
            key,
            value,
            seq,
            embedding,
            created_at: Self::parse_datetime(&created_at),
            updated_at: Self::parse_datetime(&updated_at),
        })
    }

    fn row_to_entry_info(row: &sqlx::sqlite::SqliteRow) -> Result<StoreEntryInfo, String> {
        let key_str: String = row.try_get("key").map_err(|e| e.to_string())?;
        let seq: i64 = row.try_get("seq").map_err(|e| e.to_string())?;
        let embedding_dimensions: Option<i64> = row
            .try_get("embedding_dimensions")
            .map_err(|e| e.to_string())?;
        let created_at: String = row
            .try_get("created_at")
            .unwrap_or_else(|_| Utc::now().to_rfc3339());
        let updated_at: String = row
            .try_get("updated_at")
            .unwrap_or_else(|_| Utc::now().to_rfc3339());

        let key: serde_json::Value =
            serde_json::from_str(&key_str).map_err(|e| format!("Bad key JSON: {e}"))?;

        Ok(StoreEntryInfo {
            key,
            seq,
            embedding_dimensions: embedding_dimensions.map(|d| d as usize),
            created_at: Self::parse_datetime(&created_at),
            updated_at: Self::parse_datetime(&updated_at),
        })
    }

    fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
        embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    fn parse_datetime(raw: &str) -> DateTime<Utc> {
        if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
            return dt.with_timezone(&Utc);
        }

        for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
            if let Ok(dt) = NaiveDateTime::parse_from_str(raw, format) {
                return DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc);
            }
        }

        Utc::now()
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn ensure_store(&self, config: &StoreMapConfig) -> Result<(), String> {
        let pool = self.sqlite_pool()?;
        let store_id = Uuid::now_v7();
        let embedding_conf = effective_embedding_conf(config)
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("Failed to serialize embedding_conf: {e}"))?;

        sqlx::query(
            "INSERT OR IGNORE INTO store_map (store_id, org_id, env_id, name, embedding_conf, text_search)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(store_id)
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(&config.name)
        .bind(&embedding_conf)
        .bind(config.text_search as i32)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to ensure store '{}': {e}", config.name))?;

        let row = sqlx::query(
            "SELECT embedding_conf, text_search
             FROM store_map
             WHERE org_id = ? AND env_id = ? AND name = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(&config.name)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("Failed to load store metadata '{}': {e}", config.name))?;

        let existing_conf_raw: Option<String> =
            row.try_get("embedding_conf").map_err(|e| e.to_string())?;
        let existing_conf = existing_conf_raw
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| format!("Invalid embedding_conf for store '{}': {e}", config.name))?;
        let existing_text_search: i64 = row.try_get("text_search").unwrap_or(0);
        let action = validate_store_map_compatibility(
            config,
            existing_conf.as_ref(),
            existing_text_search != 0,
        )?;

        if let super::CompatAction::UpdateConf(new_conf) = action {
            let serialized = serde_json::to_string(&new_conf)
                .map_err(|e| format!("Failed to serialize updated embedding_conf: {e}"))?;
            sqlx::query(
                "UPDATE store_map SET embedding_conf = ?
                 WHERE org_id = ? AND env_id = ? AND name = ?",
            )
            .bind(&serialized)
            .bind(self.org_id)
            .bind(self.env_id)
            .bind(&config.name)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to update embedding_conf for '{}': {e}", config.name))?;
        }

        self.ensured.lock().unwrap().insert(config.name.clone());
        Ok(())
    }

    async fn put(
        &self,
        store_name: &str,
        key: serde_json::Value,
        value: serde_json::Value,
        embedding: Option<Vec<f32>>,
        text_content: Option<String>,
    ) -> Result<StoreEntry, String> {
        let pool = self.sqlite_pool()?;
        let key_str = serde_json::to_string(&key).map_err(|e| e.to_string())?;
        let value_str = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        let embedding_bytes = embedding.as_ref().map(|e| Self::embedding_to_bytes(e));
        let now = Utc::now().to_rfc3339();

        let size: i64 = key_str.len() as i64
            + value_str.len() as i64
            + text_content.as_ref().map_or(0, |s| s.len() as i64)
            + embedding.as_ref().map_or(0, |e| (e.len() * 4) as i64);

        let result = sqlx::query(
            "INSERT INTO store_map_entry (org_id, env_id, store_name, key, value, embedding, text_content, size, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(org_id, env_id, store_name, key) DO UPDATE SET
                value = excluded.value,
                embedding = excluded.embedding,
                text_content = excluded.text_content,
                size = excluded.size,
                updated_at = excluded.updated_at
             RETURNING seq, created_at, updated_at",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(&key_str)
        .bind(&value_str)
        .bind(&embedding_bytes)
        .bind(&text_content)
        .bind(size)
        .bind(&now)
        .bind(&now)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("store put failed: {e}"))?;

        let seq: i64 = result.try_get("seq").map_err(|e| e.to_string())?;
        let created_at: String = result.try_get("created_at").unwrap_or_else(|_| now.clone());
        let updated_at: String = result.try_get("updated_at").unwrap_or(now);

        Ok(StoreEntry {
            key,
            value,
            seq,
            embedding,
            created_at: Self::parse_datetime(&created_at),
            updated_at: Self::parse_datetime(&updated_at),
        })
    }

    async fn get(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntry>, String> {
        let pool = self.sqlite_pool()?;
        let key_str = serde_json::to_string(key).map_err(|e| e.to_string())?;

        let row = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ? AND key = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(&key_str)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("store get failed: {e}"))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_entry(&r)?)),
            None => Ok(None),
        }
    }

    async fn get_info(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntryInfo>, String> {
        let pool = self.sqlite_pool()?;
        let key_str = serde_json::to_string(key).map_err(|e| e.to_string())?;

        let row = sqlx::query(
            "SELECT key, seq,
                    CASE WHEN embedding IS NULL THEN NULL ELSE length(embedding) / 4 END AS embedding_dimensions,
                    created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ? AND key = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(&key_str)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("store get_info failed: {e}"))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_entry_info(&r)?)),
            None => Ok(None),
        }
    }

    async fn delete(&self, store_name: &str, key: &serde_json::Value) -> Result<bool, String> {
        let pool = self.sqlite_pool()?;
        let key_str = serde_json::to_string(key).map_err(|e| e.to_string())?;

        let result = sqlx::query(
            "DELETE FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ? AND key = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(&key_str)
        .execute(pool)
        .await
        .map_err(|e| format!("store delete failed: {e}"))?;

        Ok(result.rows_affected() > 0)
    }

    async fn keys(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String> {
        let pool = self.sqlite_pool()?;
        let rows = sqlx::query(
            "SELECT key FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?
             ORDER BY seq",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("store keys failed: {e}"))?;

        rows.iter()
            .map(|r| {
                let s: String = r.try_get("key").map_err(|e| e.to_string())?;
                serde_json::from_str(&s).map_err(|e| format!("Bad key JSON: {e}"))
            })
            .collect()
    }

    async fn vals(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String> {
        let pool = self.sqlite_pool()?;
        let rows = sqlx::query(
            "SELECT value FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?
             ORDER BY seq",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("store vals failed: {e}"))?;

        rows.iter()
            .map(|r| {
                let s: String = r.try_get("value").map_err(|e| e.to_string())?;
                serde_json::from_str(&s).map_err(|e| format!("Bad value JSON: {e}"))
            })
            .collect()
    }

    async fn length(&self, store_name: &str) -> Result<usize, String> {
        let pool = self.sqlite_pool()?;
        let row = sqlx::query(
            "SELECT COUNT(*) as cnt FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("store length failed: {e}"))?;

        let cnt: i64 = row.try_get("cnt").map_err(|e| e.to_string())?;
        Ok(cnt as usize)
    }

    async fn first(&self, store_name: &str) -> Result<Option<StoreEntry>, String> {
        let pool = self.sqlite_pool()?;
        let row = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?
             ORDER BY seq ASC LIMIT 1",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("store first failed: {e}"))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_entry(&r)?)),
            None => Ok(None),
        }
    }

    async fn last(&self, store_name: &str) -> Result<Option<StoreEntry>, String> {
        let pool = self.sqlite_pool()?;
        let row = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("store last failed: {e}"))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_entry(&r)?)),
            None => Ok(None),
        }
    }

    async fn list(
        &self,
        store_name: &str,
        options: ListOptions,
    ) -> Result<Vec<StoreEntry>, String> {
        let pool = self.sqlite_pool()?;
        let limit = options.limit.unwrap_or(1000) as i64;
        let offset = options.offset.unwrap_or(0) as i64;

        let rows = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?
             ORDER BY seq LIMIT ? OFFSET ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("store list failed: {e}"))?;

        rows.iter().map(Self::row_to_entry).collect()
    }

    async fn list_info(
        &self,
        store_name: &str,
        options: ListOptions,
    ) -> Result<Vec<StoreEntryInfo>, String> {
        let pool = self.sqlite_pool()?;
        let limit = options.limit.unwrap_or(1000) as i64;
        let offset = options.offset.unwrap_or(0) as i64;

        let rows = sqlx::query(
            "SELECT key, seq,
                    CASE WHEN embedding IS NULL THEN NULL ELSE length(embedding) / 4 END AS embedding_dimensions,
                    created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?
             ORDER BY seq LIMIT ? OFFSET ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("store list_info failed: {e}"))?;

        rows.iter().map(Self::row_to_entry_info).collect()
    }

    async fn clear(&self, store_name: &str) -> Result<(), String> {
        let pool = self.sqlite_pool()?;
        sqlx::query(
            "DELETE FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .execute(pool)
        .await
        .map_err(|e| format!("store clear failed: {e}"))?;
        Ok(())
    }

    async fn destroy(&self, store_name: &str) -> Result<(), String> {
        let pool = self.sqlite_pool()?;
        sqlx::query(
            "DELETE FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .execute(pool)
        .await
        .map_err(|e| format!("store destroy (entries) failed: {e}"))?;

        sqlx::query(
            "DELETE FROM store_map
             WHERE org_id = ? AND env_id = ? AND name = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .execute(pool)
        .await
        .map_err(|e| format!("store destroy (meta) failed: {e}"))?;

        self.ensured.lock().unwrap().remove(store_name);
        Ok(())
    }

    async fn put_many(
        &self,
        store_name: &str,
        entries: Vec<(
            serde_json::Value,
            serde_json::Value,
            Option<Vec<f32>>,
            Option<String>,
        )>,
    ) -> Result<usize, String> {
        let mut count = 0usize;
        for (key, value, embedding, text_content) in entries {
            self.put(store_name, key, value, embedding, text_content)
                .await?;
            count += 1;
        }
        Ok(count)
    }

    async fn search(
        &self,
        store_name: &str,
        query_text: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        options: SearchOptions,
    ) -> Result<Vec<SearchResult>, String> {
        let limit = options.limit.unwrap_or(10);
        let min_score = options.min_score.unwrap_or(0.0);

        match options.mode {
            SearchMode::Semantic => {
                let qe = query_embedding.ok_or("Semantic search requires an embedding")?;
                self.brute_force_vector_search(store_name, &qe, limit, min_score)
                    .await
            }
            SearchMode::Keyword => {
                let qt = query_text.ok_or("Keyword search requires query text")?;
                self.keyword_search(store_name, qt, limit).await
            }
            SearchMode::Hybrid => {
                let mut results = Vec::new();
                if let Some(qe) = &query_embedding {
                    results.extend(
                        self.brute_force_vector_search(store_name, qe, limit, min_score)
                            .await?,
                    );
                }
                if let Some(qt) = query_text {
                    let kw = self.keyword_search(store_name, qt, limit).await?;
                    for r in kw {
                        if !results.iter().any(|e| e.entry.key == r.entry.key) {
                            results.push(r);
                        }
                    }
                }
                results.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                results.truncate(limit);
                Ok(results)
            }
        }
    }

    async fn search_info(
        &self,
        store_name: &str,
        query_text: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        search_options: SearchOptions,
        list_options: ListOptions,
    ) -> Result<SearchInfoPage, String> {
        let pool = self.sqlite_pool()?;
        let limit = list_options.limit.unwrap_or(1000);
        let offset = list_options.offset.unwrap_or(0);
        let min_score = search_options.min_score.unwrap_or(0.0);
        let query_lower = query_text
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| q.to_lowercase());
        let use_keyword = query_lower.is_some()
            && matches!(
                search_options.mode,
                SearchMode::Keyword | SearchMode::Hybrid
            );
        let use_semantic = query_embedding.is_some()
            && matches!(
                search_options.mode,
                SearchMode::Semantic | SearchMode::Hybrid
            );

        if !use_keyword && !use_semantic {
            return Ok(SearchInfoPage {
                entries: Vec::new(),
                total_entries: 0,
            });
        }

        let rows = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ?
             ORDER BY seq",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("store search_info failed: {e}"))?;

        let mut matches: Vec<(i64, f64, StoreEntryInfo)> = Vec::new();
        for row in &rows {
            let entry = Self::row_to_entry(row)?;
            let mut best_score: Option<f64> = None;

            if use_keyword && let Some(needle) = &query_lower {
                let key =
                    serde_json::to_string(&entry.key).unwrap_or_else(|_| entry.key.to_string());
                let value =
                    serde_json::to_string(&entry.value).unwrap_or_else(|_| entry.value.to_string());
                if key.to_lowercase().contains(needle) || value.to_lowercase().contains(needle) {
                    best_score = Some(1.0);
                }
            }

            if use_semantic
                && let (Some(query_embedding), Some(entry_embedding)) =
                    (query_embedding.as_ref(), entry.embedding.as_ref())
            {
                let score = cosine_similarity(query_embedding, entry_embedding);
                if score >= min_score {
                    best_score = Some(best_score.map_or(score, |current| current.max(score)));
                }
            }

            if let Some(score) = best_score {
                matches.push((entry.seq, score, entry.to_info()));
            }
        }

        if use_semantic {
            matches.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            });
        } else {
            matches.sort_by_key(|a| a.0);
        }

        let total_entries = matches.len();
        let entries = matches
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(_, _, info)| info)
            .collect();

        Ok(SearchInfoPage {
            entries,
            total_entries,
        })
    }

    async fn storage_bytes(&self) -> Result<i64, String> {
        let pool = self.sqlite_pool()?;
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(size), 0) FROM store_map_entry
             WHERE org_id = ? AND env_id = ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("storage_bytes query failed: {e}"))?;

        Ok(row.0)
    }

    fn storage_type(&self) -> &str {
        "sqlite"
    }

    async fn list_maps(&self) -> Result<Vec<StoreMapInfo>, String> {
        let pool = self.sqlite_pool()?;
        let rows = sqlx::query(
            "SELECT
                m.name AS name,
                m.embedding_conf AS embedding_conf,
                m.text_search AS text_search,
                m.created_at AS created_at,
                COALESCE((
                    SELECT COUNT(*) FROM store_map_entry e
                    WHERE e.org_id = m.org_id AND e.env_id = m.env_id AND e.store_name = m.name
                ), 0) AS entry_count,
                COALESCE((
                    SELECT SUM(size) FROM store_map_entry e
                    WHERE e.org_id = m.org_id AND e.env_id = m.env_id AND e.store_name = m.name
                ), 0) AS storage_bytes
             FROM store_map m
             WHERE m.org_id = ? AND m.env_id = ?
             ORDER BY m.name",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("store list_maps failed: {e}"))?;

        rows.iter()
            .map(|r| {
                let name: String = r.try_get("name").map_err(|e| e.to_string())?;
                let embedding_conf_raw: Option<String> =
                    r.try_get("embedding_conf").map_err(|e| e.to_string())?;
                let embedding_conf = embedding_conf_raw
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|e| format!("Invalid embedding_conf for store '{name}': {e}"))?;
                let text_search: i64 = r.try_get("text_search").unwrap_or(0);
                let entry_count: i64 = r.try_get("entry_count").map_err(|e| e.to_string())?;
                let storage_bytes: i64 = r.try_get("storage_bytes").unwrap_or(0);
                let created_at: String = r
                    .try_get("created_at")
                    .unwrap_or_else(|_| Utc::now().to_rfc3339());

                Ok(StoreMapInfo {
                    name,
                    embedding_provider: embedding_conf_provider(embedding_conf.as_ref()),
                    embedding_model: embedding_conf_model(embedding_conf.as_ref()),
                    embedding_conf: embedding_conf.clone(),
                    embedding_dimensions: embedding_conf_dimensions(embedding_conf.as_ref()),
                    embedding_field: embedding_conf_field(embedding_conf.as_ref()),
                    text_search: text_search != 0,
                    entry_count,
                    storage_bytes,
                    created_at: Self::parse_datetime(&created_at),
                })
            })
            .collect()
    }
}

// Private search helpers
impl SqliteStore {
    async fn brute_force_vector_search(
        &self,
        store_name: &str,
        query_embedding: &[f32],
        limit: usize,
        min_score: f64,
    ) -> Result<Vec<SearchResult>, String> {
        let pool = self.sqlite_pool()?;
        let rows = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ? AND embedding IS NOT NULL",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("vector search fetch failed: {e}"))?;

        let mut scored: Vec<SearchResult> = Vec::new();

        for row in &rows {
            let entry = Self::row_to_entry(row)?;
            if let Some(ref emb) = entry.embedding {
                let score = cosine_similarity(query_embedding, emb);
                if score >= min_score {
                    scored.push(SearchResult { entry, score });
                }
            }
        }

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);
        Ok(scored)
    }

    async fn keyword_search(
        &self,
        store_name: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, String> {
        let pool = self.sqlite_pool()?;
        let pattern = format!("%{query}%");

        let rows = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = ? AND env_id = ? AND store_name = ? AND text_content LIKE ?
             ORDER BY seq
             LIMIT ?",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(&pattern)
        .bind(limit as i64)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("keyword search failed: {e}"))?;

        rows.iter()
            .map(|r| {
                let entry = Self::row_to_entry(r)?;
                Ok(SearchResult { entry, score: 1.0 })
            })
            .collect()
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let x = *x as f64;
        let y = *y as f64;
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

// ---------------------------------------------------------------------------
// One-shot legacy import
// ---------------------------------------------------------------------------

/// Copy rows from a pre-2.0 single-tenant `<base_path>/store.db` (created by the
/// old SqliteStore that owned its own database file) into the main hot SQLite
/// database under the supplied `(org_id, env_id)` scope.
///
/// Idempotent: on success the legacy file is renamed to `store.db.imported` so
/// subsequent calls become no-ops. Safe to call on every CLI run.
pub async fn maybe_import_legacy_store(
    main_pool: &Arc<DatabasePool>,
    base_path: &str,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<(), String> {
    use sqlx::sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
    };
    use std::path::PathBuf;

    let dir = PathBuf::from(base_path);
    let legacy = dir.join("store.db");
    if !legacy.exists() {
        return Ok(());
    }
    let imported_marker = dir.join("store.db.imported");
    if imported_marker.exists() {
        return Ok(());
    }

    let target_pool = match main_pool.as_ref() {
        DatabasePool::Sqlite(p) => p,
        // Postgres deployments never had a local legacy file; nothing to import.
        _ => return Ok(()),
    };

    tracing::warn!(
        "Importing legacy ::hot::store data from {} into the main hot SQLite DB (org={}, env={:?})",
        legacy.display(),
        org_id,
        env_id
    );

    // Open the legacy file read-only.
    let legacy_opts = SqliteConnectOptions::new()
        .filename(&legacy)
        .read_only(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);

    let legacy_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(legacy_opts)
        .await
        .map_err(|e| format!("Failed to open legacy store {}: {e}", legacy.display()))?;

    // Pull store_map rows.
    let map_rows = sqlx::query(
        "SELECT name, embedding_model, embedding_dimensions, embedding_field, text_search, created_at FROM store_map",
    )
    .fetch_all(&legacy_pool)
    .await
    .map_err(|e| format!("Failed to read legacy store_map: {e}"))?;

    let mut tx = target_pool
        .begin()
        .await
        .map_err(|e| format!("Failed to begin import tx: {e}"))?;

    let mut maps_imported = 0usize;
    for row in &map_rows {
        let name: String = row.try_get("name").map_err(|e| e.to_string())?;
        let embedding_model: Option<String> = row.try_get("embedding_model").ok();
        let embedding_dimensions: Option<i64> = row.try_get("embedding_dimensions").ok();
        let embedding_field: Option<String> = row.try_get("embedding_field").ok();
        let embedding_conf = embedding_model
            .as_ref()
            .map(|model| {
                serde_json::json!({
                    "provider": "local",
                    "model": model,
                    "dimensions": embedding_dimensions,
                    "field": embedding_field.as_deref().unwrap_or("content"),
                    "version": 1,
                })
            })
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| format!("Failed to serialize embedding_conf for '{name}': {e}"))?;
        let text_search: i64 = row.try_get("text_search").unwrap_or(0);
        let created_at: Option<String> = row.try_get("created_at").ok();

        sqlx::query(
            "INSERT OR IGNORE INTO store_map (store_id, org_id, env_id, name, embedding_conf, text_search, created_at)
             VALUES (?, ?, ?, ?, ?, ?, COALESCE(?, current_timestamp))",
        )
        .bind(Uuid::now_v7())
        .bind(org_id)
        .bind(env_id)
        .bind(&name)
        .bind(&embedding_conf)
        .bind(text_search as i32)
        .bind(&created_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to import store_map row '{name}': {e}"))?;

        maps_imported += 1;
    }

    // Pull store_map_entry rows. We don't preserve `seq` because target table has
    // its own AUTOINCREMENT counter; insertion order is preserved by the SELECT
    // ORDER BY seq, which assigns target seq values in the same order.
    let entry_rows = sqlx::query(
        "SELECT store_name, key, value, embedding, text_content, size, created_at, updated_at
         FROM store_map_entry ORDER BY seq",
    )
    .fetch_all(&legacy_pool)
    .await
    .map_err(|e| format!("Failed to read legacy store_map_entry: {e}"))?;

    let mut entries_imported = 0usize;
    for row in &entry_rows {
        let store_name: String = row.try_get("store_name").map_err(|e| e.to_string())?;
        let key: String = row.try_get("key").map_err(|e| e.to_string())?;
        let value: String = row.try_get("value").map_err(|e| e.to_string())?;
        let embedding: Option<Vec<u8>> = row.try_get("embedding").ok();
        let text_content: Option<String> = row.try_get("text_content").ok();
        let size: i64 = row.try_get("size").unwrap_or(0);
        let created_at: Option<String> = row.try_get("created_at").ok();
        let updated_at: Option<String> = row.try_get("updated_at").ok();

        sqlx::query(
            "INSERT OR IGNORE INTO store_map_entry (org_id, env_id, store_name, key, value, embedding, text_content, size, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, COALESCE(?, current_timestamp), COALESCE(?, current_timestamp))",
        )
        .bind(org_id)
        .bind(env_id)
        .bind(&store_name)
        .bind(&key)
        .bind(&value)
        .bind(&embedding)
        .bind(&text_content)
        .bind(size)
        .bind(&created_at)
        .bind(&updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to import store_map_entry row: {e}"))?;

        entries_imported += 1;
    }

    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit legacy import: {e}"))?;

    legacy_pool.close().await;

    // Mark the legacy file as imported so we don't redo this on every run.
    if let Err(e) = std::fs::rename(&legacy, &imported_marker) {
        match std::fs::write(
            &imported_marker,
            format!(
                "Imported legacy store rows from {} but failed to rename the original file: {e}\n",
                legacy.display()
            ),
        ) {
            Ok(()) => {
                tracing::warn!(
                    "Imported legacy store rows but failed to rename {} to {}: {e}. Wrote marker file so future runs will skip re-import.",
                    legacy.display(),
                    imported_marker.display()
                );
            }
            Err(marker_err) => {
                tracing::warn!(
                    "Imported legacy store rows but failed to rename {} to {}: {e}; also failed to write marker file: {marker_err}. Future runs may retry the idempotent import.",
                    legacy.display(),
                    imported_marker.display()
                );
            }
        }
    } else {
        tracing::info!(
            "Imported {maps_imported} store map(s) and {entries_imported} entr{} from legacy {} (renamed to {}).",
            if entries_imported == 1 { "y" } else { "ies" },
            legacy.display(),
            imported_marker.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DatabasePool;
    use crate::store::{ListOptions, Store, StoreMapConfig};
    use serde_json::json;
    use sqlx::sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
    };

    /// Build an in-memory SQLite pool with all hot migrations applied (via the
    /// shared `test_db()` helper) and seed a default user/org/env. Returns
    /// `(pool, org_id, env_id)` so individual tests don't have to know about
    /// the seeding internals.
    async fn temp_pool() -> (Arc<DatabasePool>, Uuid, Uuid) {
        let pool = Arc::new(crate::db::test_db().await);
        let test_data = crate::db::insert_test_data(&pool)
            .await
            .expect("insert_test_data");
        (pool, test_data.org_id, test_data.env_id)
    }

    async fn temp_store() -> (SqliteStore, Arc<DatabasePool>, Uuid, Uuid) {
        let (pool, org_id, env_id) = temp_pool().await;
        let store = SqliteStore::new(pool.clone(), org_id, env_id);
        (store, pool, org_id, env_id)
    }

    fn plain_config(name: &str) -> StoreMapConfig {
        StoreMapConfig {
            name: name.to_string(),
            embedding_provider: None,
            embedding_model: None,
            embedding_conf: None,
            embedding_field: None,
            embedding_dimensions: None,
            text_search: false,
            embedding_on_error: None,
            embed_fn: None,
            embed_batch_fn: None,
        }
    }

    #[tokio::test]
    async fn test_put_and_get() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        let entry = store
            .put("t", json!("k1"), json!({"a": 1}), None, None)
            .await
            .unwrap();
        assert_eq!(entry.key, json!("k1"));
        assert_eq!(entry.value, json!({"a": 1}));

        let fetched = store.get("t", &json!("k1")).await.unwrap().unwrap();
        assert_eq!(fetched.value, json!({"a": 1}));
    }

    #[tokio::test]
    async fn test_get_missing_returns_none() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();
        assert!(store.get("t", &json!("missing")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_put_upsert_preserves_seq() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        let e1 = store
            .put("t", json!("k"), json!(1), None, None)
            .await
            .unwrap();
        let e2 = store
            .put("t", json!("k"), json!(2), None, None)
            .await
            .unwrap();
        assert_eq!(e1.seq, e2.seq);
        assert_eq!(
            store.get("t", &json!("k")).await.unwrap().unwrap().value,
            json!(2)
        );
    }

    #[tokio::test]
    async fn test_delete() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        store
            .put("t", json!("k"), json!(1), None, None)
            .await
            .unwrap();
        assert!(store.delete("t", &json!("k")).await.unwrap());
        assert!(!store.delete("t", &json!("k")).await.unwrap());
        assert!(store.get("t", &json!("k")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_keys_and_vals() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        store
            .put("t", json!("a"), json!(1), None, None)
            .await
            .unwrap();
        store
            .put("t", json!("b"), json!(2), None, None)
            .await
            .unwrap();
        store
            .put("t", json!("c"), json!(3), None, None)
            .await
            .unwrap();

        assert_eq!(
            store.keys("t").await.unwrap(),
            vec![json!("a"), json!("b"), json!("c")]
        );
        assert_eq!(
            store.vals("t").await.unwrap(),
            vec![json!(1), json!(2), json!(3)]
        );
    }

    #[tokio::test]
    async fn test_length() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        assert_eq!(store.length("t").await.unwrap(), 0);
        store
            .put("t", json!("a"), json!(1), None, None)
            .await
            .unwrap();
        assert_eq!(store.length("t").await.unwrap(), 1);
        store
            .put("t", json!("b"), json!(2), None, None)
            .await
            .unwrap();
        assert_eq!(store.length("t").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_first_and_last() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        assert!(store.first("t").await.unwrap().is_none());
        assert!(store.last("t").await.unwrap().is_none());

        store
            .put("t", json!("a"), json!(1), None, None)
            .await
            .unwrap();
        store
            .put("t", json!("b"), json!(2), None, None)
            .await
            .unwrap();
        store
            .put("t", json!("c"), json!(3), None, None)
            .await
            .unwrap();

        assert_eq!(store.first("t").await.unwrap().unwrap().key, json!("a"));
        assert_eq!(store.last("t").await.unwrap().unwrap().key, json!("c"));
    }

    #[tokio::test]
    async fn test_list_with_pagination() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        for i in 0..5 {
            store
                .put("t", json!(format!("k{i}")), json!(i), None, None)
                .await
                .unwrap();
        }

        let page = store
            .list(
                "t",
                ListOptions {
                    limit: Some(2),
                    offset: Some(1),
                },
            )
            .await
            .unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].key, json!("k1"));
        assert_eq!(page[1].key, json!("k2"));
    }

    #[tokio::test]
    async fn test_clear() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();
        store
            .put("t", json!("a"), json!(1), None, None)
            .await
            .unwrap();
        store
            .put("t", json!("b"), json!(2), None, None)
            .await
            .unwrap();
        store.clear("t").await.unwrap();
        assert_eq!(store.length("t").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_destroy() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();
        store
            .put("t", json!("a"), json!(1), None, None)
            .await
            .unwrap();
        store.destroy("t").await.unwrap();
        assert_eq!(store.length("t").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_put_many() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        let entries = vec![
            (json!("a"), json!(1), None, None),
            (json!("b"), json!(2), None, None),
            (json!("c"), json!(3), None, None),
        ];
        let count = store.put_many("t", entries).await.unwrap();
        assert_eq!(count, 3);
        assert_eq!(store.length("t").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn test_storage_bytes() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        assert_eq!(store.storage_bytes().await.unwrap(), 0);
        store
            .put("t", json!("k1"), json!({"data": "hello"}), None, None)
            .await
            .unwrap();
        let after_one = store.storage_bytes().await.unwrap();
        assert!(after_one > 0);
        store
            .put("t", json!("k2"), json!({"data": "world"}), None, None)
            .await
            .unwrap();
        assert!(store.storage_bytes().await.unwrap() > after_one);
    }

    #[tokio::test]
    async fn test_store_isolation() {
        let (store, _pool, _org, _env) = temp_store().await;
        store.ensure_store(&plain_config("a")).await.unwrap();
        store.ensure_store(&plain_config("b")).await.unwrap();

        store
            .put("a", json!("k"), json!(1), None, None)
            .await
            .unwrap();
        store
            .put("b", json!("k"), json!(2), None, None)
            .await
            .unwrap();

        assert_eq!(
            store.get("a", &json!("k")).await.unwrap().unwrap().value,
            json!(1)
        );
        assert_eq!(
            store.get("b", &json!("k")).await.unwrap().unwrap().value,
            json!(2)
        );
    }

    /// Two SqliteStore instances over the same pool but different (org, env)
    /// must not see each other's data.
    #[tokio::test]
    async fn test_org_env_isolation() {
        let (pool, org_a, env_a) = temp_pool().await;

        // Seed a second org via the proper helper so its FK and slug rules
        // line up with what the rest of the system expects.
        let org_b = Uuid::now_v7();
        let env_b1 = Uuid::now_v7();
        let env_b2 = Uuid::now_v7();
        crate::db::org::Org::insert_org(&pool, &org_b, "Org B", "org-b", "organization", &org_a)
            .await
            .unwrap();

        let s_a = SqliteStore::new(pool.clone(), org_a, env_a);
        let s_b1 = SqliteStore::new(pool.clone(), org_b, env_b1);
        let s_b2 = SqliteStore::new(pool.clone(), org_b, env_b2);

        for s in [&s_a, &s_b1, &s_b2] {
            s.ensure_store(&plain_config("shared")).await.unwrap();
        }

        s_a.put("shared", json!("k"), json!("from-a"), None, None)
            .await
            .unwrap();
        s_b1.put("shared", json!("k"), json!("from-b1"), None, None)
            .await
            .unwrap();
        s_b2.put("shared", json!("k"), json!("from-b2"), None, None)
            .await
            .unwrap();

        assert_eq!(s_a.length("shared").await.unwrap(), 1);
        assert_eq!(s_b1.length("shared").await.unwrap(), 1);
        assert_eq!(s_b2.length("shared").await.unwrap(), 1);
        assert_eq!(
            s_a.get("shared", &json!("k")).await.unwrap().unwrap().value,
            json!("from-a")
        );
        assert_eq!(
            s_b1.get("shared", &json!("k"))
                .await
                .unwrap()
                .unwrap()
                .value,
            json!("from-b1")
        );
        assert_eq!(
            s_b2.get("shared", &json!("k"))
                .await
                .unwrap()
                .unwrap()
                .value,
            json!("from-b2")
        );

        assert!(s_a.delete("shared", &json!("k")).await.unwrap());
        assert_eq!(s_a.length("shared").await.unwrap(), 0);
        assert_eq!(s_b1.length("shared").await.unwrap(), 1);
        assert_eq!(s_b2.length("shared").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_list_maps() {
        let (store, _pool, _org, _env) = temp_store().await;

        assert!(store.list_maps().await.unwrap().is_empty());

        store.ensure_store(&plain_config("alpha")).await.unwrap();
        store
            .ensure_store(&StoreMapConfig {
                name: "docs".to_string(),
                embedding_provider: Some("openai".to_string()),
                embedding_model: Some("text-embedding-3-small".to_string()),
                embedding_conf: Some(json!({
                    "provider": "openai",
                    "model": "text-embedding-3-small",
                    "dimensions": 1536,
                    "field": "body",
                    "version": 1,
                })),
                embedding_field: Some("body".to_string()),
                embedding_dimensions: Some(1536),
                text_search: true,
                embedding_on_error: None,
                embed_fn: None,
                embed_batch_fn: None,
            })
            .await
            .unwrap();

        store
            .put("alpha", json!("k1"), json!("v1"), None, None)
            .await
            .unwrap();
        store
            .put("alpha", json!("k2"), json!("v2"), None, None)
            .await
            .unwrap();
        store
            .put(
                "docs",
                json!("d1"),
                json!({"title": "T"}),
                None,
                Some("text".into()),
            )
            .await
            .unwrap();

        let mut maps = store.list_maps().await.unwrap();
        maps.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(maps.len(), 2);
        assert_eq!(maps[0].name, "alpha");
        assert_eq!(maps[0].entry_count, 2);
        assert!(!maps[0].text_search);
        assert!(maps[0].embedding_model.is_none());

        assert_eq!(maps[1].name, "docs");
        assert_eq!(maps[1].entry_count, 1);
        assert_eq!(
            maps[1].embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(maps[1].embedding_provider.as_deref(), Some("openai"));
        assert_eq!(maps[1].embedding_field.as_deref(), Some("body"));
        assert_eq!(maps[1].embedding_dimensions, Some(1536));
        assert!(maps[1].text_search);
    }

    #[tokio::test]
    async fn test_keyword_search() {
        let (store, _pool, _org, _env) = temp_store().await;
        let config = StoreMapConfig {
            name: "t".to_string(),
            embedding_provider: None,
            embedding_model: None,
            embedding_conf: None,
            embedding_field: None,
            embedding_dimensions: None,
            text_search: true,
            embedding_on_error: None,
            embed_fn: None,
            embed_batch_fn: None,
        };
        store.ensure_store(&config).await.unwrap();

        store
            .put(
                "t",
                json!("d1"),
                json!({"title": "Refund Policy"}),
                None,
                Some("Refunds are available within 30 days".into()),
            )
            .await
            .unwrap();
        store
            .put(
                "t",
                json!("d2"),
                json!({"title": "Shipping"}),
                None,
                Some("We ship worldwide".into()),
            )
            .await
            .unwrap();

        let results = store
            .search(
                "t",
                Some("refund"),
                None,
                crate::store::SearchOptions {
                    limit: Some(10),
                    min_score: None,
                    mode: crate::store::SearchMode::Keyword,
                },
            )
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.key, json!("d1"));
    }

    #[tokio::test]
    async fn test_cosine_similarity_fn() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
        let c = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &c).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
    }

    /// Build a v1 (pre-multi-tenant) `<dir>/store.db` file matching the original
    /// SqliteStore-owned schema and assert `maybe_import_legacy_store` copies its
    /// rows into the main pool under the supplied (org, env).
    #[tokio::test]
    async fn test_legacy_import() {
        let (pool, org_id, env_id) = temp_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("store.db");

        // Create the legacy DB.
        {
            let opts = SqliteConnectOptions::new()
                .filename(&legacy_path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal);
            let lp = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .unwrap();

            sqlx::query(
                "CREATE TABLE store_map (
                    name TEXT PRIMARY KEY,
                    embedding_model TEXT,
                    embedding_dimensions INTEGER,
                    embedding_field TEXT DEFAULT 'content',
                    text_search INTEGER DEFAULT 0,
                    created_at DATETIME DEFAULT (datetime('now'))
                )",
            )
            .execute(&lp)
            .await
            .unwrap();

            sqlx::query(
                "CREATE TABLE store_map_entry (
                    store_name TEXT NOT NULL,
                    key TEXT NOT NULL,
                    value TEXT NOT NULL,
                    seq INTEGER PRIMARY KEY AUTOINCREMENT,
                    embedding BLOB,
                    text_content TEXT,
                    size INTEGER NOT NULL DEFAULT 0,
                    created_at DATETIME DEFAULT (datetime('now')),
                    updated_at DATETIME DEFAULT (datetime('now')),
                    UNIQUE(store_name, key)
                )",
            )
            .execute(&lp)
            .await
            .unwrap();

            sqlx::query("INSERT INTO store_map (name, created_at) VALUES (?, ?)")
                .bind("legacy")
                .bind("2024-01-02 03:04:05")
                .execute(&lp)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO store_map_entry (store_name, key, value, size, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind("legacy")
            .bind("\"old-key\"")
            .bind("\"old-value\"")
            .bind(20i64)
            .bind("2024-01-03 04:05:06")
            .bind("2024-01-04 05:06:07")
            .execute(&lp)
            .await
            .unwrap();
            lp.close().await;
        }

        // Run the import.
        maybe_import_legacy_store(&pool, dir.path().to_str().unwrap(), org_id, env_id)
            .await
            .unwrap();

        // The new store under the same scope sees the legacy data.
        let store = SqliteStore::new(pool.clone(), org_id, env_id);
        let entry = store
            .get("legacy", &json!("old-key"))
            .await
            .unwrap()
            .expect("legacy row migrated");
        assert_eq!(entry.value, json!("old-value"));
        assert_eq!(
            entry.created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-01-03 04:05:06"
        );
        assert_eq!(
            entry.updated_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-01-04 05:06:07"
        );

        let maps = store.list_maps().await.unwrap();
        assert_eq!(
            maps[0].created_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-01-02 03:04:05"
        );

        // Legacy file was renamed to .imported, so a second call is a no-op.
        assert!(!dir.path().join("store.db").exists());
        assert!(dir.path().join("store.db.imported").exists());
        maybe_import_legacy_store(&pool, dir.path().to_str().unwrap(), org_id, env_id)
            .await
            .unwrap();
    }
}

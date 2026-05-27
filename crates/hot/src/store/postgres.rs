use async_trait::async_trait;
use chrono::Utc;
use sqlx::{PgPool, Row};
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

pub struct PgStore {
    pool: Arc<DatabasePool>,
    org_id: Uuid,
    env_id: Uuid,
    ensured: Mutex<HashSet<String>>,
}

impl PgStore {
    pub fn new(pool: Arc<DatabasePool>, org_id: Uuid, env_id: Uuid) -> Self {
        Self {
            pool,
            org_id,
            env_id,
            ensured: Mutex::new(HashSet::new()),
        }
    }

    fn pg_pool(&self) -> Result<&PgPool, String> {
        match self.pool.as_ref() {
            DatabasePool::Postgres(pg) => Ok(pg),
            _ => Err("PgStore requires a Postgres database pool".to_string()),
        }
    }

    fn row_to_entry(row: &sqlx::postgres::PgRow) -> Result<StoreEntry, String> {
        let key: serde_json::Value = row.try_get("key").map_err(|e| e.to_string())?;
        let value: serde_json::Value = row.try_get("value").map_err(|e| e.to_string())?;
        let seq: i64 = row.try_get("seq").map_err(|e| e.to_string())?;

        let embedding_raw: Option<Vec<u8>> = row
            .try_get::<Option<Vec<u8>>, _>("embedding_raw")
            .unwrap_or(None);
        let embedding = embedding_raw.map(|bytes| {
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        });

        let created_at: chrono::DateTime<Utc> =
            row.try_get("created_at").unwrap_or_else(|_| Utc::now());
        let updated_at: chrono::DateTime<Utc> =
            row.try_get("updated_at").unwrap_or_else(|_| Utc::now());

        Ok(StoreEntry {
            key,
            value,
            seq,
            embedding,
            created_at,
            updated_at,
        })
    }

    fn row_to_entry_info(row: &sqlx::postgres::PgRow) -> Result<StoreEntryInfo, String> {
        let key: serde_json::Value = row.try_get("key").map_err(|e| e.to_string())?;
        let seq: i64 = row.try_get("seq").map_err(|e| e.to_string())?;
        let embedding_dimensions: Option<i64> = row
            .try_get("embedding_dimensions")
            .map_err(|e| e.to_string())?;
        let created_at: chrono::DateTime<Utc> =
            row.try_get("created_at").unwrap_or_else(|_| Utc::now());
        let updated_at: chrono::DateTime<Utc> =
            row.try_get("updated_at").unwrap_or_else(|_| Utc::now());

        Ok(StoreEntryInfo {
            key,
            seq,
            embedding_dimensions: embedding_dimensions.map(|d| d as usize),
            created_at,
            updated_at,
        })
    }

    fn embedding_to_pgvector_literal(embedding: &[f32]) -> String {
        let inner: Vec<String> = embedding.iter().map(|f| f.to_string()).collect();
        format!("[{}]", inner.join(","))
    }

    fn quote_ident(identifier: &str) -> String {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    }

    fn quote_literal(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
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
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[async_trait]
impl Store for PgStore {
    async fn ensure_store(&self, config: &StoreMapConfig) -> Result<(), String> {
        let pg = self.pg_pool()?;

        sqlx::query(
            "INSERT INTO store_map (name, org_id, env_id, embedding_conf, text_search)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (org_id, env_id, name) DO NOTHING",
        )
        .bind(&config.name)
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(effective_embedding_conf(config))
        .bind(config.text_search)
        .execute(pg)
        .await
        .map_err(|e| format!("Failed to ensure store '{}': {e}", config.name))?;

        let row = sqlx::query(
            "SELECT embedding_conf, text_search
             FROM store_map
             WHERE org_id = $1 AND env_id = $2 AND name = $3",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(&config.name)
        .fetch_one(pg)
        .await
        .map_err(|e| format!("Failed to load store metadata '{}': {e}", config.name))?;

        let existing_conf: Option<serde_json::Value> =
            row.try_get("embedding_conf").map_err(|e| e.to_string())?;
        let existing_text_search: bool = row.try_get("text_search").unwrap_or(false);
        let action =
            validate_store_map_compatibility(config, existing_conf.as_ref(), existing_text_search)?;

        if let super::CompatAction::UpdateConf(new_conf) = action {
            sqlx::query(
                "UPDATE store_map SET embedding_conf = $1
                 WHERE org_id = $2 AND env_id = $3 AND name = $4",
            )
            .bind(&new_conf)
            .bind(self.org_id)
            .bind(self.env_id)
            .bind(&config.name)
            .execute(pg)
            .await
            .map_err(|e| format!("Failed to update embedding_conf for '{}': {e}", config.name))?;
        }

        // Create a per-store HNSW index for vector search if embeddings are enabled.
        // The column is dimension-less `vector`, so the index is scoped per-store via
        // a partial WHERE clause. This allows different stores to use different models.
        if config.embedding_model.is_some() {
            let hash =
                blake3::hash(format!("{}:{}:{}", self.org_id, self.env_id, config.name).as_bytes());
            let idx_name = format!("idx_sme_emb_{}", &hash.to_hex()[..24]);
            let create_idx = format!(
                "CREATE INDEX IF NOT EXISTS {} ON store_map_entry USING hnsw (embedding vector_cosine_ops) WHERE org_id = {}::uuid AND env_id = {}::uuid AND store_name = {} AND embedding IS NOT NULL",
                Self::quote_ident(&idx_name),
                Self::quote_literal(&self.org_id.to_string()),
                Self::quote_literal(&self.env_id.to_string()),
                Self::quote_literal(&config.name)
            );
            if let Err(e) = sqlx::query(&create_idx).execute(pg).await {
                tracing::warn!(
                    "Failed to create HNSW index for store '{}': {e}",
                    config.name
                );
            }
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
        let pg = self.pg_pool()?;

        let embedding_literal = embedding
            .as_ref()
            .map(|e| Self::embedding_to_pgvector_literal(e));

        let size: i64 = key.to_string().len() as i64
            + value.to_string().len() as i64
            + text_content.as_ref().map_or(0, |s| s.len() as i64)
            + embedding.as_ref().map_or(0, |e| (e.len() * 4) as i64);

        let query_str = if embedding_literal.is_some() {
            "INSERT INTO store_map_entry (org_id, env_id, store_name, key, value, embedding, text_content, size)
             VALUES ($1, $2, $3, $4, $5, $6::vector, $7, $8)
             ON CONFLICT (org_id, env_id, store_name, key) DO UPDATE SET
                value = EXCLUDED.value,
                embedding = EXCLUDED.embedding,
                text_content = EXCLUDED.text_content,
                size = EXCLUDED.size,
                updated_at = NOW()
             RETURNING seq, created_at, updated_at"
        } else {
            "INSERT INTO store_map_entry (org_id, env_id, store_name, key, value, embedding, text_content, size)
             VALUES ($1, $2, $3, $4, $5, NULL, $7, $8)
             ON CONFLICT (org_id, env_id, store_name, key) DO UPDATE SET
                value = EXCLUDED.value,
                embedding = EXCLUDED.embedding,
                text_content = EXCLUDED.text_content,
                size = EXCLUDED.size,
                updated_at = NOW()
             RETURNING seq, created_at, updated_at"
        };

        let row = sqlx::query(query_str)
            .bind(self.org_id)
            .bind(self.env_id)
            .bind(store_name)
            .bind(&key)
            .bind(&value)
            .bind(&embedding_literal)
            .bind(&text_content)
            .bind(size)
            .fetch_one(pg)
            .await
            .map_err(|e| format!("store put failed: {e}"))?;

        let seq: i64 = row.try_get("seq").map_err(|e| e.to_string())?;
        let created_at: chrono::DateTime<Utc> =
            row.try_get("created_at").unwrap_or_else(|_| Utc::now());
        let updated_at: chrono::DateTime<Utc> =
            row.try_get("updated_at").unwrap_or_else(|_| Utc::now());

        Ok(StoreEntry {
            key,
            value,
            seq,
            embedding,
            created_at,
            updated_at,
        })
    }

    async fn get(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntry>, String> {
        let pg = self.pg_pool()?;

        let row = sqlx::query(
            "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3 AND key = $4",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(key)
        .fetch_optional(pg)
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
        let pg = self.pg_pool()?;

        let row = sqlx::query(
            "SELECT key, seq,
                    CASE WHEN embedding IS NULL THEN NULL ELSE (length(embedding::bytea) / 4)::bigint END AS embedding_dimensions,
                    created_at, updated_at
             FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3 AND key = $4",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(key)
        .fetch_optional(pg)
        .await
        .map_err(|e| format!("store get_info failed: {e}"))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_entry_info(&r)?)),
            None => Ok(None),
        }
    }

    async fn delete(&self, store_name: &str, key: &serde_json::Value) -> Result<bool, String> {
        let pg = self.pg_pool()?;

        let result = sqlx::query(
            "DELETE FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3 AND key = $4",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(key)
        .execute(pg)
        .await
        .map_err(|e| format!("store delete failed: {e}"))?;

        Ok(result.rows_affected() > 0)
    }

    async fn keys(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String> {
        let pg = self.pg_pool()?;

        let rows = sqlx::query(
            "SELECT key FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3
             ORDER BY seq",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_all(pg)
        .await
        .map_err(|e| format!("store keys failed: {e}"))?;

        rows.iter()
            .map(|r| r.try_get("key").map_err(|e| e.to_string()))
            .collect()
    }

    async fn vals(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String> {
        let pg = self.pg_pool()?;

        let rows = sqlx::query(
            "SELECT value FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3
             ORDER BY seq",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_all(pg)
        .await
        .map_err(|e| format!("store vals failed: {e}"))?;

        rows.iter()
            .map(|r| r.try_get("value").map_err(|e| e.to_string()))
            .collect()
    }

    async fn length(&self, store_name: &str) -> Result<usize, String> {
        let pg = self.pg_pool()?;

        let row = sqlx::query(
            "SELECT COUNT(*) as cnt FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_one(pg)
        .await
        .map_err(|e| format!("store length failed: {e}"))?;

        let cnt: i64 = row.try_get("cnt").map_err(|e| e.to_string())?;
        Ok(cnt as usize)
    }

    async fn first(&self, store_name: &str) -> Result<Option<StoreEntry>, String> {
        let pg = self.pg_pool()?;

        let row = sqlx::query(
            "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3
             ORDER BY seq ASC LIMIT 1",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_optional(pg)
        .await
        .map_err(|e| format!("store first failed: {e}"))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_entry(&r)?)),
            None => Ok(None),
        }
    }

    async fn last(&self, store_name: &str) -> Result<Option<StoreEntry>, String> {
        let pg = self.pg_pool()?;

        let row = sqlx::query(
            "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_optional(pg)
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
        let pg = self.pg_pool()?;
        let limit = options.limit.unwrap_or(1000) as i64;
        let offset = options.offset.unwrap_or(0) as i64;

        let rows = sqlx::query(
            "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3
             ORDER BY seq LIMIT $4 OFFSET $5",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(limit)
        .bind(offset)
        .fetch_all(pg)
        .await
        .map_err(|e| format!("store list failed: {e}"))?;

        rows.iter().map(Self::row_to_entry).collect()
    }

    async fn list_info(
        &self,
        store_name: &str,
        options: ListOptions,
    ) -> Result<Vec<StoreEntryInfo>, String> {
        let pg = self.pg_pool()?;
        let limit = options.limit.unwrap_or(1000) as i64;
        let offset = options.offset.unwrap_or(0) as i64;

        let rows = sqlx::query(
            "SELECT key, seq,
                    CASE WHEN embedding IS NULL THEN NULL ELSE (length(embedding::bytea) / 4)::bigint END AS embedding_dimensions,
                    created_at, updated_at
             FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3
             ORDER BY seq LIMIT $4 OFFSET $5",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(limit)
        .bind(offset)
        .fetch_all(pg)
        .await
        .map_err(|e| format!("store list_info failed: {e}"))?;

        rows.iter().map(Self::row_to_entry_info).collect()
    }

    async fn clear(&self, store_name: &str) -> Result<(), String> {
        let pg = self.pg_pool()?;

        sqlx::query(
            "DELETE FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .execute(pg)
        .await
        .map_err(|e| format!("store clear failed: {e}"))?;

        Ok(())
    }

    async fn destroy(&self, store_name: &str) -> Result<(), String> {
        let pg = self.pg_pool()?;

        sqlx::query(
            "DELETE FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .execute(pg)
        .await
        .map_err(|e| format!("store destroy (entries) failed: {e}"))?;

        sqlx::query(
            "DELETE FROM store_map
             WHERE org_id = $1 AND env_id = $2 AND name = $3",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .execute(pg)
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
        let pg = self.pg_pool()?;
        let limit = options.limit.unwrap_or(10) as i64;
        let min_score = options.min_score.unwrap_or(0.0);

        match options.mode {
            SearchMode::Semantic => {
                let qe = query_embedding.ok_or("Semantic search requires an embedding")?;
                let vec_literal = Self::embedding_to_pgvector_literal(&qe);

                let rows = sqlx::query(
                    "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at,
                            1 - (embedding <=> $5::vector) as score
                     FROM store_map_entry
                     WHERE org_id = $1 AND env_id = $2 AND store_name = $3
                       AND embedding IS NOT NULL
                     ORDER BY embedding <=> $5::vector
                     LIMIT $4",
                )
                .bind(self.org_id)
                .bind(self.env_id)
                .bind(store_name)
                .bind(limit)
                .bind(&vec_literal)
                .fetch_all(pg)
                .await
                .map_err(|e| format!("semantic search failed: {e}"))?;

                let mut results = Vec::new();
                for r in &rows {
                    let score: f64 = r.try_get("score").unwrap_or(0.0);
                    if score >= min_score {
                        results.push(SearchResult {
                            entry: Self::row_to_entry(r)?,
                            score,
                        });
                    }
                }
                Ok(results)
            }
            SearchMode::Keyword => {
                let qt = query_text.ok_or("Keyword search requires query text")?;

                let rows = sqlx::query(
                    "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at,
                            similarity(text_content, $5) as score
                     FROM store_map_entry
                     WHERE org_id = $1 AND env_id = $2 AND store_name = $3
                       AND text_content % $5
                     ORDER BY similarity(text_content, $5) DESC
                     LIMIT $4",
                )
                .bind(self.org_id)
                .bind(self.env_id)
                .bind(store_name)
                .bind(limit)
                .bind(qt)
                .fetch_all(pg)
                .await
                .map_err(|e| format!("keyword search failed: {e}"))?;

                rows.iter()
                    .map(|r| {
                        let score: f64 = r.try_get::<f32, _>("score").unwrap_or(0.0) as f64;
                        Ok(SearchResult {
                            entry: Self::row_to_entry(r)?,
                            score,
                        })
                    })
                    .collect()
            }
            SearchMode::Hybrid => {
                let mut results = Vec::new();

                if let Some(qe) = &query_embedding {
                    let vec_literal = Self::embedding_to_pgvector_literal(qe);
                    let sem_rows = sqlx::query(
                        "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at,
                                1 - (embedding <=> $5::vector) as score
                         FROM store_map_entry
                         WHERE org_id = $1 AND env_id = $2 AND store_name = $3
                           AND embedding IS NOT NULL
                         ORDER BY embedding <=> $5::vector
                         LIMIT $4",
                    )
                    .bind(self.org_id)
                    .bind(self.env_id)
                    .bind(store_name)
                    .bind(limit)
                    .bind(&vec_literal)
                    .fetch_all(pg)
                    .await
                    .map_err(|e| format!("hybrid semantic search failed: {e}"))?;

                    for r in &sem_rows {
                        let score: f64 = r.try_get("score").unwrap_or(0.0);
                        if score >= min_score {
                            results.push(SearchResult {
                                entry: Self::row_to_entry(r)?,
                                score,
                            });
                        }
                    }
                }

                if let Some(qt) = query_text {
                    let kw_rows = sqlx::query(
                        "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at,
                                similarity(text_content, $5) as score
                         FROM store_map_entry
                         WHERE org_id = $1 AND env_id = $2 AND store_name = $3
                           AND text_content % $5
                         ORDER BY similarity(text_content, $5) DESC
                         LIMIT $4",
                    )
                    .bind(self.org_id)
                    .bind(self.env_id)
                    .bind(store_name)
                    .bind(limit)
                    .bind(qt)
                    .fetch_all(pg)
                    .await
                    .map_err(|e| format!("hybrid keyword search failed: {e}"))?;

                    for r in &kw_rows {
                        let entry = Self::row_to_entry(r)?;
                        if !results.iter().any(|e| e.entry.key == entry.key) {
                            let score: f64 = r.try_get::<f32, _>("score").unwrap_or(0.0) as f64;
                            results.push(SearchResult { entry, score });
                        }
                    }
                }

                results.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                results.truncate(limit as usize);
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
        let pg = self.pg_pool()?;
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
            "SELECT key, value, seq, embedding::bytea as embedding_raw, created_at, updated_at
             FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3
             ORDER BY seq",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .fetch_all(pg)
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
        let pg = self.pg_pool()?;

        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(size), 0)::bigint FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .fetch_one(pg)
        .await
        .map_err(|e| format!("storage_bytes query failed: {e}"))?;

        Ok(row.0)
    }

    fn storage_type(&self) -> &str {
        "postgres"
    }

    async fn list_maps(&self) -> Result<Vec<StoreMapInfo>, String> {
        let pg = self.pg_pool()?;

        let rows = sqlx::query(
            "SELECT
                m.name AS name,
                m.embedding_conf AS embedding_conf,
                m.text_search AS text_search,
                m.created_at AS created_at,
                COALESCE((
                    SELECT COUNT(*) FROM store_map_entry e
                    WHERE e.org_id = m.org_id
                      AND e.env_id = m.env_id
                      AND e.store_name = m.name
                ), 0)::bigint AS entry_count,
                COALESCE((
                    SELECT SUM(size) FROM store_map_entry e
                    WHERE e.org_id = m.org_id
                      AND e.env_id = m.env_id
                      AND e.store_name = m.name
                ), 0)::bigint AS storage_bytes
             FROM store_map m
             WHERE m.org_id = $1 AND m.env_id = $2
             ORDER BY m.name",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .fetch_all(pg)
        .await
        .map_err(|e| format!("store list_maps failed: {e}"))?;

        rows.iter()
            .map(|r| {
                let name: String = r.try_get("name").map_err(|e| e.to_string())?;
                let embedding_conf: Option<serde_json::Value> =
                    r.try_get("embedding_conf").map_err(|e| e.to_string())?;
                let text_search: bool = r.try_get("text_search").unwrap_or(false);
                let entry_count: i64 = r.try_get("entry_count").map_err(|e| e.to_string())?;
                let storage_bytes: i64 = r.try_get("storage_bytes").unwrap_or(0);
                let created_at: chrono::DateTime<Utc> =
                    r.try_get("created_at").unwrap_or_else(|_| Utc::now());

                Ok(StoreMapInfo {
                    name,
                    embedding_provider: embedding_conf_provider(embedding_conf.as_ref()),
                    embedding_model: embedding_conf_model(embedding_conf.as_ref()),
                    embedding_conf: embedding_conf.clone(),
                    embedding_dimensions: embedding_conf_dimensions(embedding_conf.as_ref()),
                    embedding_field: embedding_conf_field(embedding_conf.as_ref()),
                    text_search,
                    entry_count,
                    storage_bytes,
                    created_at,
                })
            })
            .collect()
    }
}

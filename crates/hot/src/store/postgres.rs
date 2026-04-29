use async_trait::async_trait;
use chrono::Utc;
use sqlx::{PgPool, Row};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::db::DatabasePool;

use super::{
    ListOptions, SearchMode, SearchOptions, SearchResult, Store, StoreEntry, StoreMapConfig,
};

pub struct PgStore {
    pool: Arc<DatabasePool>,
    org_id: Uuid,
    env_id: Option<Uuid>,
    ensured: Mutex<HashSet<String>>,
}

impl PgStore {
    pub fn new(pool: Arc<DatabasePool>, org_id: Uuid, env_id: Option<Uuid>) -> Self {
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

    fn embedding_to_pgvector_literal(embedding: &[f32]) -> String {
        let inner: Vec<String> = embedding.iter().map(|f| f.to_string()).collect();
        format!("[{}]", inner.join(","))
    }
}

#[async_trait]
impl Store for PgStore {
    async fn ensure_store(&self, config: &StoreMapConfig) -> Result<(), String> {
        {
            let ensured = self.ensured.lock().unwrap();
            if ensured.contains(&config.name) {
                return Ok(());
            }
        }

        let pg = self.pg_pool()?;

        sqlx::query(
            "INSERT INTO store_map (name, org_id, env_id, embedding_model, embedding_dimensions, embedding_field, text_search)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (org_id, env_id, name) DO NOTHING",
        )
        .bind(&config.name)
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(&config.embedding_model)
        .bind(config.embedding_dimensions.map(|d| d as i32))
        .bind(config.embedding_field.as_deref().unwrap_or("content"))
        .bind(config.text_search)
        .execute(pg)
        .await
        .map_err(|e| format!("Failed to ensure store '{}': {e}", config.name))?;

        // Create a per-store HNSW index for vector search if embeddings are enabled.
        // The column is dimension-less `vector`, so the index is scoped per-store via
        // a partial WHERE clause. This allows different stores to use different models.
        if config.embedding_model.is_some() {
            let safe_name = config.name.replace(['-', ' '], "_");
            let idx_name = format!("idx_sme_emb_{}", safe_name);
            let create_idx = format!(
                "CREATE INDEX IF NOT EXISTS {} ON store_map_entry USING hnsw (embedding vector_cosine_ops) WHERE store_name = '{}' AND embedding IS NOT NULL",
                idx_name, config.name
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
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3 AND key = $4",
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

    async fn delete(&self, store_name: &str, key: &serde_json::Value) -> Result<bool, String> {
        let pg = self.pg_pool()?;

        let result = sqlx::query(
            "DELETE FROM store_map_entry
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3 AND key = $4",
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
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3",
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
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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

    async fn clear(&self, store_name: &str) -> Result<(), String> {
        let pg = self.pg_pool()?;

        sqlx::query(
            "DELETE FROM store_map_entry
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3",
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
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .execute(pg)
        .await
        .map_err(|e| format!("store destroy (entries) failed: {e}"))?;

        sqlx::query(
            "DELETE FROM store_map
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND name = $3",
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
                     WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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
                     WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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
                         WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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
                         WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 AND store_name = $3
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

    async fn storage_bytes(&self) -> Result<i64, String> {
        let pg = self.pg_pool()?;

        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(size), 0)::bigint FROM store_map_entry
             WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2",
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
}

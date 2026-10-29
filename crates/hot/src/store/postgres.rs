use async_trait::async_trait;
use chrono::Utc;
use sqlx::Row;
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
        let pool =
            Arc::new(pool.with_scope(crate::db::DatabaseScope::Environment { org_id, env_id }));
        Self {
            pool,
            org_id,
            env_id,
            ensured: Mutex::new(HashSet::new()),
        }
    }

    fn pg_pool(&self) -> Result<&crate::db::ScopedPgPool, String> {
        match self.pool.as_ref() {
            DatabasePool::Postgres(pg) => Ok(pg),
            _ => Err("PgStore requires a Postgres database pool".to_string()),
        }
    }

    /// All writes in a batch share one account lock and commit together.
    async fn write_batch(
        &self,
        store_name: &str,
        entries: Vec<super::StoreWrite>,
        only_missing: bool,
    ) -> Result<Vec<StoreEntry>, String> {
        let (mut tx, mut budget) = super::quota::begin_postgres(
            &self.pool,
            self.org_id,
            self.env_id,
            super::quota::QuotaKind::Store,
        )
        .await?;
        let mut written = Vec::with_capacity(entries.len());
        for write in entries {
            let old:Option<i64>=sqlx::query_scalar("SELECT size FROM store_map_entry WHERE org_id=$1 AND env_id=$2 AND store_name=$3 AND key=$4")
                .bind(self.org_id).bind(self.env_id).bind(store_name).bind(&write.0).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
            if only_missing && old.is_some() {
                continue;
            }
            if let Some(entry) = self
                .write_entry(&mut tx, &mut budget, store_name, write, old, only_missing)
                .await?
            {
                written.push(entry);
            }
        }
        tx.commit()
            .await
            .map_err(|e| format!("store commit failed: {e}"))?;
        Ok(written)
    }

    /// Lock the entry at `key` for the rest of `tx` and return its stored
    /// value and size. A missing key locks nothing: a write that creates it
    /// must insert with ON CONFLICT DO NOTHING and check that it won.
    async fn lock_entry(
        &self,
        tx: &mut sqlx::PgConnection,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<(serde_json::Value, i64)>, String> {
        sqlx::query_as(
            "SELECT value, size FROM store_map_entry WHERE org_id=$1 AND env_id=$2 AND store_name=$3 AND key=$4 FOR UPDATE",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())
    }

    /// Write one entry whose stored size was `old` (`None` when missing),
    /// after admitting its size to `budget`. With `only_missing` a concurrent
    /// creator wins: nothing is written, the admission is returned, and this
    /// gives `None`.
    async fn write_entry(
        &self,
        tx: &mut sqlx::PgConnection,
        budget: &mut super::quota::Budget,
        store_name: &str,
        (key, value, embedding, text_content): super::StoreWrite,
        old: Option<i64>,
        only_missing: bool,
    ) -> Result<Option<StoreEntry>, String> {
        let embedding_data = embedding
            .as_ref()
            .map(|e| Self::embedding_to_pgvector_literal(e));
        let now = Utc::now();
        let embedding_bytes = embedding
            .as_ref()
            .map_or(Some(0), |e| e.len().checked_mul(4))
            .ok_or("Store embedding is too large")?;
        let size = super::quota::entry_size(
            key.to_string().len(),
            value.to_string().len(),
            text_content.as_ref().map_or(0, |s| s.len()),
            embedding_bytes,
        )?;
        budget.replace(old.unwrap_or(0), size)?;
        let query = if only_missing {
            "INSERT INTO store_map_entry (org_id, env_id, store_name, key, value, embedding, text_content, size, created_at, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6::vector,$7,$8,$9,$9)
             ON CONFLICT (org_id,env_id,store_name,key) DO NOTHING
             RETURNING seq,created_at,updated_at"
        } else {
            "INSERT INTO store_map_entry (org_id, env_id, store_name, key, value, embedding, text_content, size, created_at, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6::vector,$7,$8,$9,$9)
             ON CONFLICT (org_id,env_id,store_name,key) DO UPDATE SET value=EXCLUDED.value,embedding=EXCLUDED.embedding,text_content=EXCLUDED.text_content,size=EXCLUDED.size,updated_at=EXCLUDED.updated_at
             RETURNING seq,created_at,updated_at"
        };
        let row = sqlx::query(query)
            .bind(self.org_id)
            .bind(self.env_id)
            .bind(store_name)
            .bind(&key)
            .bind(&value)
            .bind(&embedding_data)
            .bind(&text_content)
            .bind(size)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| format!("store write failed: {e}"))?;
        let Some(row) = row else {
            budget.replace(size, old.unwrap_or(0))?;
            return Ok(None);
        };
        let seq: i64 = row.try_get("seq").map_err(|e| e.to_string())?;
        let created_at: chrono::DateTime<Utc> =
            row.try_get("created_at").map_err(|e| e.to_string())?;
        let updated_at: chrono::DateTime<Utc> =
            row.try_get("updated_at").map_err(|e| e.to_string())?;
        Ok(Some(StoreEntry {
            key,
            value,
            seq,
            embedding,
            created_at,
            updated_at,
        }))
    }

    /// The range `[low, high)` of key text that holds the string keys
    /// starting with `prefix`, in byte order (`high` is `None` when nothing
    /// is above them). It is read from `idx_store_map_entry_key_prefix`: the
    /// jsonb key itself orders strings by the database collation, where the
    /// keys sharing a prefix need not be one range.
    fn prefix_range(prefix: Option<&str>) -> Option<(String, Option<String>)> {
        prefix.map(|prefix| (prefix.to_string(), super::prefix_upper_bound(prefix)))
    }

    /// A page of `columns` in insertion order, binding org, env, store name,
    /// limit and offset, then the prefix range's ends when there is one.
    fn listing_sql(columns: &str, range: &Option<(String, Option<String>)>) -> String {
        let terms = match range {
            None => "",
            Some((_, None)) => {
                " AND jsonb_typeof(key) = 'string' AND (key #>> '{}') COLLATE \"C\" >= $6"
            }
            Some((_, Some(_))) => {
                " AND jsonb_typeof(key) = 'string' AND (key #>> '{}') COLLATE \"C\" >= $6 AND (key #>> '{}') COLLATE \"C\" < $7"
            }
        };
        format!(
            "SELECT {columns}
             FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3{terms}
             ORDER BY seq LIMIT $4 OFFSET $5"
        )
    }

    fn row_to_entry(row: &sqlx::postgres::PgRow) -> Result<StoreEntry, String> {
        let key: serde_json::Value = row.try_get("key").map_err(|e| e.to_string())?;
        let value: serde_json::Value = row.try_get("value").map_err(|e| e.to_string())?;
        let seq: i64 = row.try_get("seq").map_err(|e| e.to_string())?;

        let embedding_text: Option<String> = row
            .try_get::<Option<String>, _>("embedding_text")
            .unwrap_or(None);
        let embedding = embedding_text
            .as_deref()
            .map(Self::parse_pgvector_text)
            .transpose()?;

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

    fn parse_pgvector_text(raw: &str) -> Result<Vec<f32>, String> {
        let inner = raw
            .trim()
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(|| format!("Invalid pgvector text literal: {raw}"))?;

        if inner.trim().is_empty() {
            return Ok(Vec::new());
        }

        inner
            .split(',')
            .map(|part| {
                part.trim()
                    .parse::<f32>()
                    .map_err(|e| format!("Invalid pgvector element '{part}': {e}"))
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

        // Index DDL is performed by the separately credentialed resource
        // provisioner. Runtime connections must not own customer-data tables.

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
        self.write_batch(
            store_name,
            vec![(key, value, embedding, text_content)],
            false,
        )
        .await?
        .pop()
        .ok_or_else(|| "Store write returned no entry".into())
    }

    async fn put_if_missing(
        &self,
        store_name: &str,
        key: serde_json::Value,
        value: serde_json::Value,
        embedding: Option<Vec<f32>>,
        text_content: Option<String>,
    ) -> Result<bool, String> {
        Ok(!self
            .write_batch(
                store_name,
                vec![(key, value, embedding, text_content)],
                true,
            )
            .await?
            .is_empty())
    }

    async fn put_if(
        &self,
        store_name: &str,
        key: serde_json::Value,
        expected: serde_json::Value,
        value: serde_json::Value,
        embedding: Option<Vec<f32>>,
        text_content: Option<String>,
    ) -> Result<bool, String> {
        let (mut tx, mut budget) = super::quota::begin_postgres(
            &self.pool,
            self.org_id,
            self.env_id,
            super::quota::QuotaKind::Store,
        )
        .await?;
        let stored = self.lock_entry(&mut tx, store_name, &key).await?;
        if !super::stored_value_matches(stored.as_ref().map(|(value, _)| value), &expected) {
            return Ok(false);
        }
        let old = stored.map(|(_, size)| size);
        let write = (key, value, embedding, text_content);
        // Creating a missing key races other creators; losing means the
        // key now holds their value, not the expected one.
        let wrote = self
            .write_entry(&mut tx, &mut budget, store_name, write, old, old.is_none())
            .await?
            .is_some();
        tx.commit()
            .await
            .map_err(|e| format!("store commit failed: {e}"))?;
        Ok(wrote)
    }

    async fn increment(
        &self,
        store_name: &str,
        key: serde_json::Value,
        by: i64,
    ) -> Result<i64, String> {
        let (mut tx, mut budget) = super::quota::begin_postgres(
            &self.pool,
            self.org_id,
            self.env_id,
            super::quota::QuotaKind::Store,
        )
        .await?;
        // A concurrent increment can create a missing key between the lock
        // and the insert. It has committed once our insert yields to it, and
        // each statement here reads the latest commit, so the next lock sees
        // its row. Only a delete racing every retry can exhaust them.
        for _ in 0..8 {
            let stored = self.lock_entry(&mut tx, store_name, &key).await?;
            let next = super::incremented(stored.as_ref().map(|(value, _)| value), by)?;
            let old = stored.map(|(_, size)| size);
            let write = (key.clone(), serde_json::Value::from(next), None, None);
            if self
                .write_entry(&mut tx, &mut budget, store_name, write, old, old.is_none())
                .await?
                .is_some()
            {
                tx.commit()
                    .await
                    .map_err(|e| format!("store commit failed: {e}"))?;
                return Ok(next);
            }
        }
        Err("store increment kept losing its entry to concurrent writers".into())
    }

    async fn get(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntry>, String> {
        let pg = self.pg_pool()?;

        let row = sqlx::query(
            "SELECT key, value, seq, embedding::text as embedding_text, created_at, updated_at
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
                    CASE WHEN embedding IS NULL THEN NULL ELSE vector_dims(embedding)::bigint END AS embedding_dimensions,
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
            "SELECT key, value, seq, embedding::text as embedding_text, created_at, updated_at
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
            "SELECT key, value, seq, embedding::text as embedding_text, created_at, updated_at
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
        let range = Self::prefix_range(options.prefix.as_deref());

        let rows = sqlx::query(sqlx::AssertSqlSafe(Self::listing_sql(
            "key, value, seq, embedding::text as embedding_text, created_at, updated_at",
            &range,
        )))
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(limit)
        .bind(offset);
        let rows = match &range {
            Some((low, Some(high))) => rows.bind(low).bind(high),
            Some((low, None)) => rows.bind(low),
            None => rows,
        }
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
        let range = Self::prefix_range(options.prefix.as_deref());

        let rows = sqlx::query(sqlx::AssertSqlSafe(Self::listing_sql(
            "key, seq,
                    CASE WHEN embedding IS NULL THEN NULL ELSE vector_dims(embedding)::bigint END AS embedding_dimensions,
                    created_at, updated_at",
            &range,
        )))
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(limit)
        .bind(offset);
        let rows = match &range {
            Some((low, Some(high))) => rows.bind(low).bind(high),
            Some((low, None)) => rows.bind(low),
            None => rows,
        }
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
        entries: Vec<super::StoreWrite>,
    ) -> Result<usize, String> {
        Ok(self.write_batch(store_name, entries, false).await?.len())
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

                let dimensions = qe.len();
                if dimensions == 0 {
                    return Err("Semantic search requires a nonempty embedding".into());
                }
                let statement = format!("SELECT key, value, seq, embedding::text as embedding_text, created_at, updated_at,
                            1 - (embedding::vector({dimensions}) <=> $5::vector({dimensions})) as score
                     FROM store_map_entry
                     WHERE org_id = $1 AND env_id = $2 AND store_name = $3
                       AND embedding IS NOT NULL AND vector_dims(embedding) = {dimensions}
                     ORDER BY embedding::vector({dimensions}) <=> $5::vector({dimensions})
                     LIMIT $4");
                let rows = sqlx::query(sqlx::AssertSqlSafe(statement.as_str()))
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
                    "SELECT key, value, seq, embedding::text as embedding_text, created_at, updated_at,
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
                    let dimensions = qe.len();
                    if dimensions == 0 {
                        return Err("Semantic search requires a nonempty embedding".into());
                    }
                    let statement = format!("SELECT key, value, seq, embedding::text as embedding_text, created_at, updated_at,
                                1 - (embedding::vector({dimensions}) <=> $5::vector({dimensions})) as score
                         FROM store_map_entry
                         WHERE org_id = $1 AND env_id = $2 AND store_name = $3
                           AND embedding IS NOT NULL AND vector_dims(embedding) = {dimensions}
                         ORDER BY embedding::vector({dimensions}) <=> $5::vector({dimensions})
                         LIMIT $4");
                    let sem_rows = sqlx::query(sqlx::AssertSqlSafe(statement.as_str()))
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
                        "SELECT key, value, seq, embedding::text as embedding_text, created_at, updated_at,
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
            "SELECT key, value, seq, embedding::text as embedding_text, created_at, updated_at
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, StoreMapConfig};
    use serde_json::json;

    async fn postgres_store() -> Result<Option<PgStore>, String> {
        let uri = match std::env::var("HOT_TEST_POSTGRES_URI") {
            Ok(uri) => uri,
            Err(_) => {
                eprintln!("skipping: HOT_TEST_POSTGRES_URI is not set");
                return Ok(None);
            }
        };
        let schema =
            std::env::var("HOT_TEST_POSTGRES_SCHEMA").unwrap_or_else(|_| "hot".to_string());
        let conf = crate::val!({
            "uri": uri,
            "schema": schema,
            // This concurrency fixture uses the dedicated admin test URI.
            // Production role qualification is exercised by postgres_rls.
            "rls": { "enforce": false },
        });
        crate::db::run_migrations(&conf)
            .await
            .map_err(|e| format!("Postgres fixture migrations: {e}"))?;
        let pool = Arc::new(
            crate::db::create_db_pool(&conf)
                .await
                .map_err(|e| format!("Postgres fixture pool: {e}"))?,
        );
        // The supplied database can survive repeated test runs. Local-dev
        // defaults use fixed unique emails/slugs, so give this fixture its own
        // identity rather than colliding with a previous run's records.
        let user = Uuid::now_v7();
        let org = Uuid::now_v7();
        let env = Uuid::now_v7();
        let onboarding = pool.with_scope(crate::db::DatabaseScope::Bootstrap(
            crate::db::BootstrapOperation::Onboarding,
        ));
        crate::db::User::insert_user(
            &onboarding,
            &user,
            &format!("store-{user}@example.test"),
            None,
            Some(&user),
        )
        .await
        .map_err(|e| e.to_string())?;
        crate::db::Org::insert_org(
            &onboarding,
            &org,
            "Store fixture",
            &format!("store-{org}"),
            "individual",
            &user,
        )
        .await
        .map_err(|e| e.to_string())?;
        crate::db::Env::insert_env(&onboarding, &env, &org, "development", &user)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Some(PgStore::new(pool, org, env)))
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

    #[test]
    fn parses_pgvector_text_literal() {
        assert_eq!(
            PgStore::parse_pgvector_text("[1,2.5,-3]").unwrap(),
            vec![1.0, 2.5, -3.0]
        );
        assert_eq!(
            PgStore::parse_pgvector_text("[ ]").unwrap(),
            Vec::<f32>::new()
        );
    }

    #[test]
    fn rejects_invalid_pgvector_text_literal() {
        assert!(PgStore::parse_pgvector_text("1,2,3").is_err());
        assert!(PgStore::parse_pgvector_text("[1,nope]").is_err());
    }

    #[test]
    fn postgres_store_queries_do_not_cast_vector_to_bytea() {
        let source = include_str!("postgres.rs");

        assert!(!source.contains(concat!("embedding::", "bytea")));
        assert!(!source.contains(concat!("embedding_", "raw")));
        assert!(source.contains("embedding::text as embedding_text"));
        assert!(source.contains("vector_dims(embedding)::bigint"));
    }

    #[tokio::test]
    async fn postgres_put_if_missing_has_one_concurrent_winner() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("insert-test-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        let store = Arc::new(store);

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let store = store.clone();
                let name = name.clone();
                tokio::spawn(async move {
                    store
                        .put_if_missing(&name, json!("race"), json!(i), None, None)
                        .await
                })
            })
            .collect();

        let mut winners = 0;
        for handle in handles {
            if handle.await.map_err(|e| e.to_string())?? {
                winners += 1;
            }
        }

        assert_eq!(winners, 1);
        assert_eq!(store.length(&name).await?, 1);
        assert!(
            !store
                .put_if_missing(&name, json!("race"), json!("replacement"), None, None)
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn postgres_put_if_compares_values_as_hot_values() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("put-if-test-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        let key = || json!("case");

        assert!(
            store
                .put_if(
                    &name,
                    key(),
                    serde_json::Value::Null,
                    json!({"zeta": 1, "alpha": [1.5]}),
                    None,
                    None
                )
                .await?
        );
        assert!(
            !store
                .put_if(&name, key(), serde_json::Value::Null, json!(2), None, None)
                .await?
        );
        // jsonb hands the object back with its keys in its own order.
        let stored = store.get(&name, &key()).await?.ok_or("missing")?.value;
        assert!(
            store
                .put_if(&name, key(), stored.clone(), json!("next"), None, None)
                .await?
        );
        assert!(
            !store
                .put_if(&name, key(), stored, json!(3), None, None)
                .await?
        );
        assert_eq!(
            store.get(&name, &key()).await?.ok_or("missing")?.value,
            json!("next")
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn postgres_concurrent_put_if_keeps_every_entry() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("put-if-race-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        crate::store::contention::put_if_keeps_every_entry(Arc::new(store), &name).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn postgres_concurrent_increments_count_exactly() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("increment-race-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        let store = Arc::new(store);
        crate::store::contention::increments_count_exactly(store.clone(), &name).await?;
        store
            .put(&name, json!("text"), json!("x"), None, None)
            .await?;
        assert!(store.increment(&name, json!("text"), 1).await.is_err());
        assert_eq!(
            store
                .get(&name, &json!("text"))
                .await?
                .ok_or("missing")?
                .value,
            json!("x")
        );
        Ok(())
    }

    #[tokio::test]
    async fn postgres_prefix_listing_reads_the_key_prefix_index() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("prefix-test-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        let mut writes: Vec<super::super::StoreWrite> = (0..2000)
            .map(|i| (json!(format!("other:{i:04}")), json!(i), None, None))
            .collect();
        for key in [
            json!("case:1:b"),
            json!("case:10"),
            json!("case:1:a"),
            json!("case;"),
            json!(17),
            json!("say \"case:1:\""),
            json!("Case:1:z"),
        ] {
            writes.push((key, json!(1), None, None));
        }
        store.put_many(&name, writes).await?;
        let keys = |prefix: &str, limit, offset| {
            let options = ListOptions {
                limit,
                offset,
                prefix: Some(prefix.to_string()),
            };
            let (store, name) = (&store, &name);
            async move {
                let entries = store.list(name, options.clone()).await?;
                let infos = store.list_info(name, options).await?;
                let keys: Vec<_> = entries.into_iter().map(|e| e.key).collect();
                assert_eq!(keys, infos.into_iter().map(|i| i.key).collect::<Vec<_>>());
                Ok::<_, String>(keys)
            }
        };
        assert_eq!(
            keys("case:1:", None, None).await?,
            [json!("case:1:b"), json!("case:1:a")]
        );
        assert_eq!(keys("case:1", Some(1), Some(1)).await?, [json!("case:10")]);
        assert_eq!(
            keys("say \"", None, None).await?,
            [json!("say \"case:1:\"")]
        );
        assert_eq!(keys("", Some(3000), None).await?.len(), 2006);

        let range = PgStore::prefix_range(Some("case:1:"));
        let (low, high) = range.clone().ok_or("no range")?;
        let sql = format!("EXPLAIN {}", PgStore::listing_sql("key", &range));
        let mut tx = store.pg_pool()?.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("ANALYZE store_map_entry")
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(store.org_id)
            .bind(store.env_id)
            .bind(&name)
            .bind(10i64)
            .bind(0i64)
            .bind(low)
            .bind(high)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        assert!(
            plan.iter()
                .any(|step| step.contains("idx_store_map_entry_key_prefix")),
            "the prefix read should use the key prefix index: {plan:?}"
        );
        Ok(())
    }
}

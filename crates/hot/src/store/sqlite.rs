use async_trait::async_trait;
use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use super::{
    ListOptions, SearchMode, SearchOptions, SearchResult, Store, StoreEntry, StoreMapConfig,
};

pub struct SqliteStore {
    pool: SqlitePool,
    /// Track which stores have been ensured so we don't re-run schema per call.
    ensured: Mutex<HashSet<String>>,
}

impl SqliteStore {
    pub async fn new(base_path: &str) -> Result<Self, String> {
        let dir = PathBuf::from(base_path);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create store directory {}: {e}", dir.display()))?;

        let db_path = dir.join("store.db");

        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(30));

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(|e| format!("Failed to open store database: {e}"))?;

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .map_err(|e| format!("Failed to enable foreign keys: {e}"))?;

        // Create the metadata table (stores which named maps exist and their config).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS store_map (
                name TEXT PRIMARY KEY,
                embedding_model TEXT,
                embedding_dimensions INTEGER,
                embedding_field TEXT DEFAULT 'content',
                text_search INTEGER DEFAULT 0,
                created_at DATETIME DEFAULT (datetime('now'))
            )",
        )
        .execute(&pool)
        .await
        .map_err(|e| format!("Failed to create store_map table: {e}"))?;

        // Create the entry table shared by all maps. Scoped by store_name.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS store_map_entry (
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
        .execute(&pool)
        .await
        .map_err(|e| format!("Failed to create store_map_entry table: {e}"))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_store_entry_order
             ON store_map_entry(store_name, seq)",
        )
        .execute(&pool)
        .await
        .map_err(|e| format!("Failed to create seq index: {e}"))?;

        Ok(Self {
            pool,
            ensured: Mutex::new(HashSet::new()),
        })
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
            created_at: chrono::DateTime::parse_from_rfc3339(&created_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            updated_at: chrono::DateTime::parse_from_rfc3339(&updated_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    }

    fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
        embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn ensure_store(&self, config: &StoreMapConfig) -> Result<(), String> {
        {
            let ensured = self.ensured.lock().unwrap();
            if ensured.contains(&config.name) {
                return Ok(());
            }
        }

        sqlx::query(
            "INSERT OR IGNORE INTO store_map (name, embedding_model, embedding_dimensions, embedding_field, text_search)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&config.name)
        .bind(&config.embedding_model)
        .bind(config.embedding_dimensions.map(|d| d as i32))
        .bind(config.embedding_field.as_deref().unwrap_or("content"))
        .bind(config.text_search as i32)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("Failed to ensure store '{}': {e}", config.name))?;

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
        let key_str = serde_json::to_string(&key).map_err(|e| e.to_string())?;
        let value_str = serde_json::to_string(&value).map_err(|e| e.to_string())?;
        let embedding_bytes = embedding.as_ref().map(|e| Self::embedding_to_bytes(e));
        let now = Utc::now().to_rfc3339();

        let size: i64 = key_str.len() as i64
            + value_str.len() as i64
            + text_content.as_ref().map_or(0, |s| s.len() as i64)
            + embedding.as_ref().map_or(0, |e| (e.len() * 4) as i64);

        let result = sqlx::query(
            "INSERT INTO store_map_entry (store_name, key, value, embedding, text_content, size, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(store_name, key) DO UPDATE SET
                value = excluded.value,
                embedding = excluded.embedding,
                text_content = excluded.text_content,
                size = excluded.size,
                updated_at = excluded.updated_at
             RETURNING seq, created_at, updated_at",
        )
        .bind(store_name)
        .bind(&key_str)
        .bind(&value_str)
        .bind(&embedding_bytes)
        .bind(&text_content)
        .bind(size)
        .bind(&now)
        .bind(&now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("store put failed: {e}"))?;

        let seq: i64 = result.try_get("seq").map_err(|e| e.to_string())?;

        Ok(StoreEntry {
            key,
            value,
            seq,
            embedding,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
    }

    async fn get(
        &self,
        store_name: &str,
        key: &serde_json::Value,
    ) -> Result<Option<StoreEntry>, String> {
        let key_str = serde_json::to_string(key).map_err(|e| e.to_string())?;

        let row = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry WHERE store_name = ? AND key = ?",
        )
        .bind(store_name)
        .bind(&key_str)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("store get failed: {e}"))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_entry(&r)?)),
            None => Ok(None),
        }
    }

    async fn delete(&self, store_name: &str, key: &serde_json::Value) -> Result<bool, String> {
        let key_str = serde_json::to_string(key).map_err(|e| e.to_string())?;

        let result = sqlx::query("DELETE FROM store_map_entry WHERE store_name = ? AND key = ?")
            .bind(store_name)
            .bind(&key_str)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("store delete failed: {e}"))?;

        Ok(result.rows_affected() > 0)
    }

    async fn keys(&self, store_name: &str) -> Result<Vec<serde_json::Value>, String> {
        let rows = sqlx::query("SELECT key FROM store_map_entry WHERE store_name = ? ORDER BY seq")
            .bind(store_name)
            .fetch_all(&self.pool)
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
        let rows =
            sqlx::query("SELECT value FROM store_map_entry WHERE store_name = ? ORDER BY seq")
                .bind(store_name)
                .fetch_all(&self.pool)
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
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM store_map_entry WHERE store_name = ?")
            .bind(store_name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("store length failed: {e}"))?;

        let cnt: i64 = row.try_get("cnt").map_err(|e| e.to_string())?;
        Ok(cnt as usize)
    }

    async fn first(&self, store_name: &str) -> Result<Option<StoreEntry>, String> {
        let row = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry WHERE store_name = ? ORDER BY seq ASC LIMIT 1",
        )
        .bind(store_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("store first failed: {e}"))?;

        match row {
            Some(r) => Ok(Some(Self::row_to_entry(&r)?)),
            None => Ok(None),
        }
    }

    async fn last(&self, store_name: &str) -> Result<Option<StoreEntry>, String> {
        let row = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry WHERE store_name = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(store_name)
        .fetch_optional(&self.pool)
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
        let limit = options.limit.unwrap_or(1000) as i64;
        let offset = options.offset.unwrap_or(0) as i64;

        let rows = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry WHERE store_name = ?
             ORDER BY seq LIMIT ? OFFSET ?",
        )
        .bind(store_name)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("store list failed: {e}"))?;

        rows.iter().map(Self::row_to_entry).collect()
    }

    async fn clear(&self, store_name: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM store_map_entry WHERE store_name = ?")
            .bind(store_name)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("store clear failed: {e}"))?;
        Ok(())
    }

    async fn destroy(&self, store_name: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM store_map_entry WHERE store_name = ?")
            .bind(store_name)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("store destroy (entries) failed: {e}"))?;

        sqlx::query("DELETE FROM store_map WHERE name = ?")
            .bind(store_name)
            .execute(&self.pool)
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

    async fn storage_bytes(&self) -> Result<i64, String> {
        let row: (i64,) = sqlx::query_as("SELECT COALESCE(SUM(size), 0) FROM store_map_entry")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| format!("storage_bytes query failed: {e}"))?;

        Ok(row.0)
    }

    fn storage_type(&self) -> &str {
        "sqlite"
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
        let rows = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry WHERE store_name = ? AND embedding IS NOT NULL",
        )
        .bind(store_name)
        .fetch_all(&self.pool)
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
        let pattern = format!("%{query}%");

        let rows = sqlx::query(
            "SELECT key, value, seq, embedding, created_at, updated_at
             FROM store_map_entry
             WHERE store_name = ? AND text_content LIKE ?
             ORDER BY seq
             LIMIT ?",
        )
        .bind(store_name)
        .bind(&pattern)
        .bind(limit as i64)
        .fetch_all(&self.pool)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ListOptions, Store, StoreMapConfig};
    use serde_json::json;

    async fn temp_store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = SqliteStore::new(dir.path().to_str().unwrap())
            .await
            .expect("create store");
        (store, dir)
    }

    fn plain_config(name: &str) -> StoreMapConfig {
        StoreMapConfig {
            name: name.to_string(),
            embedding_model: None,
            embedding_field: None,
            embedding_dimensions: None,
            text_search: false,
        }
    }

    #[tokio::test]
    async fn test_put_and_get() {
        let (store, _dir) = temp_store().await;
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
        let (store, _dir) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();
        assert!(store.get("t", &json!("missing")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_put_upsert_preserves_seq() {
        let (store, _dir) = temp_store().await;
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
        let (store, _dir) = temp_store().await;
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
        let (store, _dir) = temp_store().await;
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

        let keys = store.keys("t").await.unwrap();
        assert_eq!(keys, vec![json!("a"), json!("b"), json!("c")]);

        let vals = store.vals("t").await.unwrap();
        assert_eq!(vals, vec![json!(1), json!(2), json!(3)]);
    }

    #[tokio::test]
    async fn test_length() {
        let (store, _dir) = temp_store().await;
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
        let (store, _dir) = temp_store().await;
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

        let first = store.first("t").await.unwrap().unwrap();
        assert_eq!(first.key, json!("a"));
        let last = store.last("t").await.unwrap().unwrap();
        assert_eq!(last.key, json!("c"));
    }

    #[tokio::test]
    async fn test_list_with_pagination() {
        let (store, _dir) = temp_store().await;
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
        let (store, _dir) = temp_store().await;
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
        let (store, _dir) = temp_store().await;
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
        let (store, _dir) = temp_store().await;
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
        let (store, _dir) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        assert_eq!(store.storage_bytes().await.unwrap(), 0);

        store
            .put("t", json!("key1"), json!({"data": "hello"}), None, None)
            .await
            .unwrap();
        let bytes_after_one = store.storage_bytes().await.unwrap();
        assert!(
            bytes_after_one > 0,
            "storage_bytes should be positive after a put"
        );

        store
            .put("t", json!("key2"), json!({"data": "world"}), None, None)
            .await
            .unwrap();
        let bytes_after_two = store.storage_bytes().await.unwrap();
        assert!(
            bytes_after_two > bytes_after_one,
            "storage_bytes should grow after another put"
        );

        store.delete("t", &json!("key1")).await.unwrap();
        let bytes_after_delete = store.storage_bytes().await.unwrap();
        assert!(
            bytes_after_delete < bytes_after_two,
            "storage_bytes should shrink after delete"
        );
    }

    #[tokio::test]
    async fn test_storage_bytes_includes_embeddings() {
        let (store, _dir) = temp_store().await;
        store.ensure_store(&plain_config("t")).await.unwrap();

        let embedding = vec![0.1f32; 384];
        store
            .put(
                "t",
                json!("k"),
                json!("v"),
                Some(embedding),
                Some("text content".into()),
            )
            .await
            .unwrap();

        let bytes = store.storage_bytes().await.unwrap();
        // embedding: 384 * 4 = 1536 bytes, plus key/value/text
        assert!(bytes > 1536, "storage_bytes should account for embeddings");
    }

    #[tokio::test]
    async fn test_store_isolation() {
        let (store, _dir) = temp_store().await;
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

        assert_eq!(store.length("a").await.unwrap(), 1);
        assert_eq!(store.length("b").await.unwrap(), 1);
        assert_eq!(
            store.get("a", &json!("k")).await.unwrap().unwrap().value,
            json!(1)
        );
        assert_eq!(
            store.get("b", &json!("k")).await.unwrap().unwrap().value,
            json!(2)
        );
    }

    #[tokio::test]
    async fn test_keyword_search() {
        let (store, _dir) = temp_store().await;
        let config = StoreMapConfig {
            name: "t".to_string(),
            embedding_model: None,
            embedding_field: None,
            embedding_dimensions: None,
            text_search: true,
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
}

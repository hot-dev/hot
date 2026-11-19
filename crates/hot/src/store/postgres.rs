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
    /// Each statement writes up to [`BATCH_ROWS`] entries, so a batch costs a
    /// fixed number of round trips for every thousand entries, not two for
    /// each entry.
    async fn write_batch(
        &self,
        store_name: &str,
        entries: Vec<super::StoreWrite>,
        only_missing: bool,
    ) -> Result<Vec<StoreEntry>, String> {
        let count = entries.len();
        let mut statements: Vec<BatchRows> = Vec::new();
        for write in entries {
            match statements.last_mut() {
                Some(rows) if rows.has_room() => rows.push(write)?,
                _ => {
                    let mut rows = BatchRows::default();
                    rows.push(write)?;
                    statements.push(rows);
                }
            }
        }
        // A transaction that holds more than one entry can meet another one
        // in a deadlock, which PostgreSQL breaks by ending one of the two.
        // That one has written nothing, so it is safe to write again.
        let mut deadlocks = 0;
        let outcomes = loop {
            match self
                .write_statements(store_name, &statements, only_missing)
                .await
            {
                Err(failed) if failed.deadlock && deadlocks < DEADLOCK_RETRIES => deadlocks += 1,
                Err(failed) => return Err(failed.message),
                Ok(outcomes) => break outcomes,
            }
        };
        let mut written = Vec::with_capacity(count);
        for (rows, outcomes) in statements.into_iter().zip(outcomes) {
            rows.into_entries(outcomes, &mut written);
        }
        Ok(written)
    }

    /// Write every statement's rows in one transaction, and give what each
    /// statement reported for its rows.
    async fn write_statements(
        &self,
        store_name: &str,
        statements: &[BatchRows],
        only_missing: bool,
    ) -> Result<Vec<Vec<RowOutcome>>, WriteFailed> {
        let (mut tx, mut budget) = super::quota::begin_postgres(
            &self.pool,
            self.org_id,
            self.env_id,
            super::quota::QuotaKind::Store,
        )
        .await?;
        let now = Utc::now();
        let mut outcomes = Vec::with_capacity(statements.len());
        for rows in statements {
            let reported = self
                .write_rows(&mut tx, store_name, rows, only_missing, now)
                .await?;
            rows.admit(&reported, &mut budget)?;
            outcomes.push(reported);
        }
        tx.commit()
            .await
            .map_err(|e| format!("store commit failed: {e}"))?;
        Ok(outcomes)
    }

    /// Write `rows` with one statement, and give what it reported for each
    /// row it wrote, in the order of the rows.
    ///
    /// The statement reads no entry before it writes: a condition on the
    /// jsonb key is not leakproof, so under row-level security a read by key
    /// filters every entry of the store. The insert finds an existing entry
    /// through the primary key instead, and hands back its old size and value.
    ///
    /// One insert cannot touch a row twice, so of the rows naming one key it
    /// writes only one: the last, or the first with `only_missing`, where the
    /// others find the key present. PostgreSQL decides which keys are the
    /// same, since jsonb holds `1` and `1.0` to be one key. An entry keeps the
    /// key and the place of its first row, as it does when each row is
    /// written in turn, and every row still gets its own result. With
    /// `only_missing` a key that exists, here or from a concurrent creator,
    /// gives no result.
    ///
    /// Entries take their `seq` in the order of the rows, but are written,
    /// and so locked, in the order of their keys: two statements that write
    /// some of the same keys then never wait for each other in a circle.
    async fn write_rows(
        &self,
        tx: &mut sqlx::PgConnection,
        store_name: &str,
        rows: &BatchRows,
        only_missing: bool,
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<RowOutcome>, WriteFailed> {
        let (chosen, conflict, joined) = if only_missing {
            ("prev_ord IS NULL", "DO NOTHING", " AND i.prev_ord IS NULL")
        } else {
            (
                "next_ord IS NULL",
                "DO UPDATE SET value=EXCLUDED.value,embedding=EXCLUDED.embedding,text_content=EXCLUDED.text_content,size=EXCLUDED.size,updated_at=EXCLUDED.updated_at",
                "",
            )
        };
        let statement = format!(
            "WITH input AS (
                 SELECT w.*, first_value(w.key) OVER same_key AS first_key,
                        first_value(w.ord) OVER same_key AS first_ord,
                        lag(w.ord) OVER same_key AS prev_ord, lead(w.ord) OVER same_key AS next_ord
                 FROM UNNEST($4::jsonb[], $5::jsonb[], $6::text[], $7::text[], $8::bigint[])
                      WITH ORDINALITY AS w(key, value, embedding, text_content, size, ord)
                 WINDOW same_key AS (PARTITION BY w.key ORDER BY w.ord)
             ), placed AS (
                 SELECT c.*, nextval((SELECT pg_get_serial_sequence('store_map_entry', 'seq'))::regclass) AS seq
                 FROM (SELECT * FROM input WHERE {chosen} ORDER BY first_ord) c
             ), written AS (
                 INSERT INTO store_map_entry (org_id, env_id, store_name, key, value, embedding, text_content, size, seq, created_at, updated_at)
                 SELECT $1, $2, $3, first_key, value, embedding::vector, text_content, size, seq, $9, $9
                 FROM placed ORDER BY first_key
                 ON CONFLICT (org_id,env_id,store_name,key) {conflict}
                 RETURNING key, seq, created_at, updated_at, old.size AS old_size,
                           CASE WHEN old.size >= $10 THEN old.value END AS replaced
             )
             SELECT i.ord, i.prev_ord, w.seq, w.created_at, w.updated_at, w.old_size, w.replaced
             FROM input i JOIN written w ON w.key = i.key{joined}
             ORDER BY i.ord"
        );
        let outcomes: Vec<RowOutcome> = sqlx::query_as(sqlx::AssertSqlSafe(statement))
            .bind(self.org_id)
            .bind(self.env_id)
            .bind(store_name)
            .bind(&rows.keys)
            .bind(&rows.values)
            .bind(&rows.embedding_literals)
            .bind(&rows.text_contents)
            .bind(&rows.sizes)
            .bind(now)
            .bind(super::SPILLED_ENTRY_MIN_SIZE)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| WriteFailed {
                // 40P01 is deadlock_detected.
                deadlock: matches!(&e, sqlx::Error::Database(db) if db.code().as_deref() == Some("40P01")),
                message: format!("store write failed: {e}"),
            })?;
        if !only_missing && outcomes.len() != rows.keys.len() {
            return Err("Store write returned no entry".to_string().into());
        }
        Ok(outcomes)
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
            super::stored_value_size(&value),
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
            replaced: None,
        }))
    }

    /// The range `[low, high)` of key text that holds the string keys
    /// starting with `prefix`, in byte order (`high` is `None` when nothing
    /// is above them). It is read from `idx_store_map_entry_key_prefix` on
    /// `key_text`: the jsonb key itself orders strings by the database
    /// collation, where the keys sharing a prefix need not be one range, and
    /// a condition on a jsonb operator cannot be an index condition under
    /// row-level security.
    fn prefix_range(prefix: Option<&str>) -> Option<(String, Option<String>)> {
        prefix.map(|prefix| (prefix.to_string(), super::prefix_upper_bound(prefix)))
    }

    /// A page of `columns` in insertion order, binding org, env, store name,
    /// limit and offset, then the prefix range's ends when there is one.
    fn listing_sql(columns: &str, range: &Option<(String, Option<String>)>) -> String {
        let terms = match range {
            None => "",
            Some((_, None)) => " AND key_text >= $6",
            Some((_, Some(_))) => " AND key_text >= $6 AND key_text < $7",
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
            replaced: None,
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

/// Entries one statement writes at most. Their columns travel as arrays, one
/// bind each, so the count is not bounded by PostgreSQL's 65,535 binds.
const BATCH_ROWS: usize = 1000;

/// Entry bytes after which a statement takes no further entry: the arrays of
/// one statement are one message, which PostgreSQL holds in memory whole.
const BATCH_BYTES: i64 = 16 * 1024 * 1024;

/// How often a batch is written again after PostgreSQL ended its transaction
/// to break a deadlock.
const DEADLOCK_RETRIES: usize = 4;

/// What a statement reports for one row it wrote: the row's place among the
/// statement's rows, counted from 1, and the place of the row before it with
/// the same key; then the entry's `seq`, `created_at` and `updated_at`; then
/// the size of the entry it replaced, and that entry's value when it was
/// large enough to be a blob reference.
type RowOutcome = (
    i64,
    Option<i64>,
    i64,
    chrono::DateTime<Utc>,
    chrono::DateTime<Utc>,
    Option<i64>,
    Option<serde_json::Value>,
);

/// Why a batch was not written, and whether it only lost a deadlock.
struct WriteFailed {
    message: String,
    deadlock: bool,
}

impl From<String> for WriteFailed {
    fn from(message: String) -> Self {
        Self {
            message,
            deadlock: false,
        }
    }
}

/// The entries of one statement, a column at a time.
#[derive(Default)]
struct BatchRows {
    keys: Vec<serde_json::Value>,
    values: Vec<serde_json::Value>,
    embeddings: Vec<Option<Vec<f32>>>,
    embedding_literals: Vec<Option<String>>,
    text_contents: Vec<Option<String>>,
    sizes: Vec<i64>,
    bytes: i64,
}

impl BatchRows {
    fn has_room(&self) -> bool {
        self.keys.len() < BATCH_ROWS && self.bytes < BATCH_BYTES
    }

    fn push(
        &mut self,
        (key, value, embedding, text_content): super::StoreWrite,
    ) -> Result<(), String> {
        let embedding_bytes = embedding
            .as_ref()
            .map_or(Some(0), |e| e.len().checked_mul(4))
            .ok_or("Store embedding is too large")?;
        let size = super::quota::entry_size(
            key.to_string().len(),
            super::stored_value_size(&value),
            text_content.as_ref().map_or(0, |s| s.len()),
            embedding_bytes,
        )?;
        // A reference counts for the blob it points at, which stays where it is.
        if !super::spill::is_spilled(&value) {
            self.bytes = self.bytes.saturating_add(size);
        }
        self.embedding_literals.push(
            embedding
                .as_ref()
                .map(|e| PgStore::embedding_to_pgvector_literal(e)),
        );
        self.keys.push(key);
        self.values.push(value);
        self.embeddings.push(embedding);
        self.text_contents.push(text_content);
        self.sizes.push(size);
        Ok(())
    }

    /// The size of the entry that the row reported by `outcome` replaced: what
    /// the row before it wrote for the same key, or else what was stored.
    fn replaced_size(&self, outcome: &RowOutcome) -> Option<i64> {
        match outcome.1 {
            Some(before) => Some(self.sizes[before as usize - 1]),
            None => outcome.5,
        }
    }

    /// Admit each written row to `budget` in turn, as if it had been written
    /// alone: a later row for the same key replaces what the row before it
    /// wrote. A row that was not written is not admitted.
    fn admit(
        &self,
        outcomes: &[RowOutcome],
        budget: &mut super::quota::Budget,
    ) -> Result<(), String> {
        for outcome in outcomes {
            let old = self.replaced_size(outcome).unwrap_or(0);
            budget.replace(old, self.sizes[outcome.0 as usize - 1])?;
        }
        Ok(())
    }

    /// Add the entries `outcomes` report as written to `written`, in the
    /// order of the rows.
    fn into_entries(self, outcomes: Vec<RowOutcome>, written: &mut Vec<StoreEntry>) {
        let first = written.len();
        let mut outcomes = outcomes.into_iter().peekable();
        let writes = self
            .keys
            .into_iter()
            .zip(self.values)
            .zip(self.embeddings)
            .enumerate();
        for (index, ((key, value), embedding)) in writes {
            let Some((_, before, seq, created_at, updated_at, _, replaced)) =
                outcomes.next_if(|outcome| outcome.0 == index as i64 + 1)
            else {
                continue;
            };
            // Only a batch that replaces writes a key twice, and it writes
            // every row, so the row before is at its own place in `written`.
            let replaced = match before.map(|ord| ord as usize - 1) {
                Some(before) if self.sizes[before] >= super::SPILLED_ENTRY_MIN_SIZE => {
                    Some(written[first + before].value.clone())
                }
                Some(_) => None,
                None => replaced,
            };
            written.push(StoreEntry {
                key,
                value,
                seq,
                embedding,
                created_at,
                updated_at,
                replaced,
            });
        }
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
    ) -> Result<super::BatchWritten, String> {
        Ok(super::BatchWritten::of(
            self.write_batch(store_name, entries, false).await?,
        ))
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

    async fn spilled_values(
        &self,
        store_name: &str,
        key: Option<&serde_json::Value>,
    ) -> Result<Vec<serde_json::Value>, String> {
        sqlx::query_scalar(
            "SELECT value FROM store_map_entry
             WHERE org_id = $1 AND env_id = $2 AND store_name = $3 AND size >= $4
               AND ($5::jsonb IS NULL OR key = $5)",
        )
        .bind(self.org_id)
        .bind(self.env_id)
        .bind(store_name)
        .bind(super::SPILLED_ENTRY_MIN_SIZE)
        .bind(key)
        .fetch_all(self.pg_pool()?)
        .await
        .map_err(|e| format!("store read failed: {e}"))
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

    #[tokio::test]
    async fn postgres_decs_round_trip() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("dec-test-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        crate::store::contention::decs_round_trip(Arc::new(store), &name).await
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

    /// A value that counts as `size` bytes in blob storage, as the spilling
    /// store writes for a value too large to keep inline.
    fn blob_reference(size: i64) -> serde_json::Value {
        json!({"$type": crate::blob::BLOB_REF_TYPE, "$val": {"id": Uuid::now_v7(), "size": size}})
    }

    async fn listed_keys(store: &PgStore, name: &str) -> Result<Vec<serde_json::Value>, String> {
        let options = ListOptions {
            limit: Some(10_000),
            ..Default::default()
        };
        Ok(store
            .list(name, options)
            .await?
            .into_iter()
            .map(|entry| entry.key)
            .collect())
    }

    #[tokio::test]
    async fn postgres_batch_writes_every_field_in_entry_order() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("batch-fields-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        let writes: Vec<super::super::StoreWrite> = vec![
            (
                json!("zeta"),
                json!({"n": 1}),
                Some(vec![1.0, 0.5]),
                Some("first text".into()),
            ),
            (json!("alpha"), serde_json::Value::Null, None, None),
            (json!(17), json!("int key"), None, Some("third".into())),
            (
                json!({"b": 1, "a": [2]}),
                json!([1, 2]),
                Some(vec![3.0]),
                None,
            ),
        ];
        let sizes = [
            6 + 7 + 10 + 8,
            7 + 4,
            2 + 9 + 5,
            super::super::quota::entry_size(15, 5, 0, 4)?,
        ];
        let entries = store.write_batch(&name, writes.clone(), false).await?;
        assert_eq!(entries.len(), writes.len());
        for (entry, write) in entries.iter().zip(&writes) {
            assert_eq!((&entry.key, &entry.value), (&write.0, &write.1));
            assert_eq!(entry.embedding, write.2);
            assert_eq!(entry.created_at, entry.updated_at);
            assert_eq!(entry.replaced, None);
        }
        assert!(entries.windows(2).all(|pair| pair[0].seq < pair[1].seq));

        let listed = store.list(&name, ListOptions::default()).await?;
        for (stored, entry) in listed.iter().zip(&entries) {
            assert_eq!((&stored.key, &stored.value), (&entry.key, &entry.value));
            assert_eq!(stored.seq, entry.seq);
            assert_eq!(stored.embedding, entry.embedding);
            assert_eq!(stored.created_at, entry.created_at);
        }
        let columns: Vec<(Option<String>, i64)> = sqlx::query_as(
            "SELECT text_content, size FROM store_map_entry WHERE org_id=$1 AND env_id=$2 AND store_name=$3 ORDER BY seq",
        )
        .bind(store.org_id)
        .bind(store.env_id)
        .bind(&name)
        .fetch_all(store.pg_pool()?)
        .await
        .map_err(|e| e.to_string())?;
        assert_eq!(
            columns,
            writes
                .iter()
                .map(|write| write.3.clone())
                .zip(sizes)
                .collect::<Vec<_>>()
        );

        // Writing them again changes each entry in place.
        let again = store.write_batch(&name, writes, false).await?;
        for (after, before) in again.iter().zip(&entries) {
            assert_eq!(after.seq, before.seq);
            assert_eq!(after.created_at, before.created_at);
            assert!(after.updated_at > before.updated_at);
        }
        assert_eq!(store.length(&name).await?, 4);
        Ok(())
    }

    #[tokio::test]
    async fn postgres_batch_keeps_entry_order_across_statements() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("batch-order-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        // More than one statement's worth, in an order no index would give,
        // with one key in the first statement and again in the last.
        let count = BATCH_ROWS * 2 + 500;
        let mut writes: Vec<super::super::StoreWrite> = (0..count)
            .rev()
            .map(|i| (json!(format!("key:{i:05}")), json!(i), None, None))
            .collect();
        let keys: Vec<_> = writes.iter().map(|write| write.0.clone()).collect();
        writes.push((keys[3].clone(), json!("again"), None, None));
        assert_eq!(store.put_many(&name, writes).await?.count, count + 1);
        assert_eq!(listed_keys(&store, &name).await?, keys);
        assert_eq!(
            store.get(&name, &keys[3]).await?.ok_or("missing")?.value,
            json!("again")
        );
        Ok(())
    }

    #[tokio::test]
    async fn postgres_batch_naming_a_key_twice_keeps_its_last_value() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("batch-twice-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        let dec_one: serde_json::Value = serde_json::from_str("1.0").map_err(|e| e.to_string())?;
        let (first, second) = (blob_reference(40 << 20), blob_reference(41 << 20));
        let entries = store
            .write_batch(
                &name,
                vec![
                    (json!("a"), first.clone(), None, None),
                    (json!("b"), json!(2), None, None),
                    (json!("a"), second.clone(), None, None),
                    (json!(1), json!("int"), None, None),
                    // jsonb holds 1 and 1.0 to be the same key.
                    (dec_one, json!("dec"), None, None),
                    (json!("a"), json!(4), None, None),
                ],
                false,
            )
            .await?;
        assert_eq!(entries.len(), 6);
        let seqs: Vec<i64> = entries.iter().map(|entry| entry.seq).collect();
        assert_eq!((seqs[0], seqs[0]), (seqs[2], seqs[5]));
        assert_eq!(seqs[3], seqs[4]);
        assert!(seqs[0] < seqs[1] && seqs[1] < seqs[3]);
        // Each row replaced what the row before it wrote for its key.
        let replaced: Vec<_> = entries.iter().map(|entry| entry.replaced.clone()).collect();
        assert_eq!(
            replaced,
            [None, None, Some(first), None, None, Some(second)]
        );
        assert_eq!(
            listed_keys(&store, &name).await?,
            [json!("a"), json!("b"), json!(1)]
        );
        assert_eq!(
            store.get(&name, &json!("a")).await?.ok_or("missing")?.value,
            json!(4)
        );
        assert_eq!(
            store.get(&name, &json!(1)).await?.ok_or("missing")?.value,
            json!("dec")
        );
        // Three entries are stored, each counted once, as its last row: the
        // entry at 1 keeps that key and counts the `1.0` it was last given.
        assert_eq!(store.storage_bytes().await?, (3 + 1) + (3 + 1) + (3 + 5));

        // A later batch gets the stored reference back when it replaces it.
        let third = blob_reference(40 << 20);
        store
            .put(&name, json!("b"), third.clone(), None, None)
            .await?;
        let entries = store
            .write_batch(&name, vec![(json!("b"), json!(0), None, None)], false)
            .await?;
        assert_eq!(entries[0].replaced, Some(third));
        Ok(())
    }

    #[tokio::test]
    async fn postgres_batch_of_missing_keys_skips_the_keys_present() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("batch-missing-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        let kept = store.put(&name, json!("a"), json!(1), None, None).await?;
        let entries = store
            .write_batch(
                &name,
                vec![
                    (json!("a"), json!(2), None, None),
                    (json!("b"), json!(3), None, None),
                    (json!("b"), json!(4), None, None),
                    (json!("c"), json!(5), None, None),
                ],
                true,
            )
            .await?;
        let written: Vec<_> = entries
            .iter()
            .map(|entry| (entry.key.clone(), entry.value.clone()))
            .collect();
        assert_eq!(written, [(json!("b"), json!(3)), (json!("c"), json!(5))]);
        assert!(entries[0].seq < entries[1].seq);
        let stored = store.list(&name, ListOptions::default()).await?;
        let stored: Vec<_> = stored
            .into_iter()
            .map(|entry| (entry.key, entry.value, entry.seq))
            .collect();
        assert_eq!(
            stored,
            [
                (json!("a"), json!(1), kept.seq),
                (json!("b"), json!(3), entries[0].seq),
                (json!("c"), json!(5), entries[1].seq),
            ]
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn postgres_batch_locks_in_key_order_and_is_written_again_after_a_deadlock()
    -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("batch-deadlock-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        let store = Arc::new(store);
        let write = |key: &str, value: i64| (json!(key), json!(value), None, None);
        store
            .put_many(&name, vec![write("a", 0), write("b", 0)])
            .await?;
        // Whether a row of this store was locked: `lock` waits for it, and
        // `SKIP LOCKED` passes over it.
        let lock = |wait: &str| {
            format!(
                "SELECT 1 FROM store_map_entry WHERE org_id=$1 AND env_id=$2 AND store_name=$3 AND key=$4 FOR UPDATE {wait}"
            )
        };
        let key = |sql: String, key: &'static str| {
            sqlx::query_scalar::<_, i32>(sqlx::AssertSqlSafe(sql))
                .bind(store.org_id)
                .bind(store.env_id)
                .bind(name.clone())
                .bind(json!(key))
        };

        // Another transaction holds `b`, and waits long enough that
        // PostgreSQL ends the batch, not it, to break the deadlock below.
        let mut other = store.pg_pool()?.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("SET LOCAL deadlock_timeout = '60s'")
            .execute(&mut *other)
            .await
            .map_err(|e| e.to_string())?;
        key(lock(""), "b")
            .fetch_one(&mut *other)
            .await
            .map_err(|e| e.to_string())?;

        // The batch names `b` first, but takes `a` first and then waits.
        let batch = {
            let (store, name) = (store.clone(), name.clone());
            tokio::spawn(async move {
                store
                    .put_many(&name, vec![write("b", 2), write("a", 1)])
                    .await
            })
        };
        let waiting = async {
            loop {
                let free = key(lock("SKIP LOCKED"), "a")
                    .fetch_optional(store.pg_pool()?)
                    .await
                    .map_err(|e| e.to_string())?;
                if free.is_none() {
                    return Ok::<_, String>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), waiting)
            .await
            .map_err(|_| "the batch should lock `a` before it waits for `b`")??;

        // Now each waits for the other. This returns once PostgreSQL has
        // ended the batch's transaction, which then starts over and waits.
        key(lock(""), "a")
            .fetch_one(&mut *other)
            .await
            .map_err(|e| e.to_string())?;
        other.commit().await.map_err(|e| e.to_string())?;

        assert_eq!(batch.await.map_err(|e| e.to_string())??.count, 2);
        let stored = store.list(&name, ListOptions::default()).await?;
        let stored: Vec<_> = stored.into_iter().map(|e| (e.key, e.value)).collect();
        assert_eq!(stored, [(json!("a"), json!(1)), (json!("b"), json!(2))]);
        Ok(())
    }

    #[tokio::test]
    async fn postgres_batch_admits_each_entry_to_the_storage_limit() -> Result<(), String> {
        let Some(store) = postgres_store().await? else {
            return Ok(());
        };
        let name = format!("batch-quota-{}", Uuid::now_v7());
        store.ensure_store(&plain_config(&name)).await?;
        sqlx::query("UPDATE org SET features=$1 WHERE org_id=$2")
            .bind(json!({"store_storage_bytes": 14}))
            .bind(store.org_id)
            .execute(store.pg_pool()?)
            .await
            .map_err(|e| e.to_string())?;
        let write = |key: &str, value: serde_json::Value| (json!(key), value, None, None);
        let value = |key: &'static str| {
            let (store, name) = (&store, &name);
            async move { Ok::<_, String>(store.get(name, &json!(key)).await?.map(|e| e.value)) }
        };
        // A one-letter string key is 3 bytes; "v" is 3 more.
        let filled = store
            .put_many(&name, vec![write("a", json!("v")), write("b", json!("v"))])
            .await?;
        assert_eq!(filled.count, 2);

        let refused = |error: Result<super::super::BatchWritten, String>| {
            assert!(error.unwrap_err().contains("limit reached"));
        };
        // 12 bytes are used. Growing one entry by 3 passes the limit.
        refused(store.put_many(&name, vec![write("a", json!("vvvv"))]).await);
        // A batch that fails leaves none of its entries, the ones it had
        // already replaced included.
        refused(
            store
                .put_many(
                    &name,
                    vec![write("a", json!(0)), write("c", json!("too large"))],
                )
                .await,
        );
        assert_eq!(value("a").await?, Some(json!("v")));
        assert_eq!(value("c").await?, None);
        // Each row is admitted in turn: the first here needs room the second
        // would have given back.
        refused(
            store
                .put_many(&name, vec![write("c", json!("vvv")), write("a", json!(0))])
                .await,
        );
        refused(
            store
                .put_many(
                    &name,
                    vec![write("c", json!("vvvvvv")), write("c", json!(0))],
                )
                .await,
        );
        // Shrinking an entry first makes the room: 12 - 2, then + 4.
        let fits = store
            .put_many(&name, vec![write("a", json!(0)), write("c", json!(0))])
            .await?;
        assert_eq!(fits.count, 2);
        assert_eq!(store.storage_bytes().await?, 14);
        // A key that exists is skipped before it is admitted.
        assert!(
            !store
                .put_if_missing(&name, json!("a"), json!("much larger"), None, None)
                .await?
        );
        assert!(
            store
                .put_if_missing(&name, json!("d"), json!(0), None, None)
                .await
                .is_err()
        );
        let counted: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(store_bytes),0)::bigint FROM usage_storage WHERE env_id=$1",
        )
        .bind(store.env_id)
        .fetch_one(store.pg_pool()?)
        .await
        .map_err(|e| e.to_string())?;
        assert_eq!(counted, 14);
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
        // Plan as a runtime login would: one without BYPASSRLS, so forced
        // row-level security applies. Only leakproof conditions can be index
        // conditions then. The role is rolled back with the transaction.
        let role = format!("hot_prefix_plan_{}", Uuid::now_v7().simple());
        let schema: String = sqlx::query_scalar(
            "SELECT n.nspname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = 'store_map_entry'::regclass",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        // Both interpolated names are identifier-safe: a generated one, and
        // the fixture schema quoted.
        for statement in [
            format!("CREATE ROLE {role} NOLOGIN NOSUPERUSER NOBYPASSRLS"),
            format!("GRANT USAGE ON SCHEMA \"{schema}\" TO {role}"),
            format!("GRANT SELECT ON store_map_entry TO {role}"),
            format!("SET LOCAL ROLE {role}"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
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
        assert!(
            plan.iter().any(|step| step.contains("Index Cond")
                && step.contains("key_text >=")
                && step.contains("key_text <")),
            "under row-level security the prefix range should be an index condition: {plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|step| step.contains("Filter") && step.contains("key_text")),
            "the prefix range should not be a filter: {plan:?}"
        );
        Ok(())
    }
}

//! Store values too large to keep inline. A value over
//! [`MAX_INLINE_HARD_LIMIT_BYTES`] as JSON goes whole into one blob, and its
//! entry holds a reference that every read replaces with the value, so a store
//! backend only ever sees small values and the size to account for.
//!
//! Every write makes its own reference. A row never goes back to a reference
//! it has held, so releasing the one an overwrite replaced can't touch a value
//! a concurrent writer just stored. Losing that race can only leave a reference
//! behind, which is safer than the reverse.

use async_trait::async_trait;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::{
    ListOptions, SearchInfoPage, SearchOptions, SearchResult, Store, StoreEntry, StoreEntryInfo,
    StoreMapConfig, StoreMapInfo, StoreWrite,
};
use crate::blob::{BlobRefVal, BlobScope, BlobStore, MAX_INLINE_HARD_LIMIT_BYTES, RehydrateUsage};
use crate::db::DatabasePool;
use crate::val::Val;

/// Whether a stored value is a reference to a blob. Values that look like one
/// are stored in a blob themselves, so this only finds real ones.
pub(crate) fn is_spilled(value: &JsonValue) -> bool {
    value.get("$type").and_then(JsonValue::as_str) == Some(crate::blob::BLOB_REF_TYPE)
}

/// The id of the reference `value` holds.
fn ref_id(value: &JsonValue) -> Option<Uuid> {
    let val = serde_json::from_value::<Val>(value.clone()).ok()?;
    Some(BlobRefVal::from_val(&val).ok()??.id)
}

pub struct SpillingStore {
    inner: Box<dyn Store>,
    conf: Val,
    db: Arc<DatabasePool>,
    org_id: Uuid,
    env_id: Uuid,
    /// Built when first needed: most stores never hold a value this large.
    blob_store: OnceCell<Option<Arc<BlobStore>>>,
}

impl SpillingStore {
    pub fn new(
        inner: Box<dyn Store>,
        conf: Val,
        db: Arc<DatabasePool>,
        org_id: Uuid,
        env_id: Uuid,
    ) -> Self {
        Self {
            inner,
            conf,
            db,
            org_id,
            env_id,
            blob_store: OnceCell::new(),
        }
    }

    async fn blob_store(&self) -> Option<&Arc<BlobStore>> {
        self.blob_store
            .get_or_init(|| crate::blob::blob_store_from_conf(self.db.clone(), &self.conf))
            .await
            .as_ref()
    }

    fn scope(&self) -> BlobScope {
        BlobScope {
            org_id: self.org_id,
            env_id: Some(self.env_id),
            run_id: None,
        }
    }

    /// What to write for `value`: itself, or a reference to it in blob storage.
    /// Without blob storage it goes inline, and the database has the last word.
    async fn stored_form(&self, store_name: &str, value: &JsonValue) -> Result<JsonValue, String> {
        let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
        if text.len() <= MAX_INLINE_HARD_LIMIT_BYTES && !is_spilled(value) {
            return Ok(value.clone());
        }
        let Some(store) = self.blob_store().await else {
            return Ok(value.clone());
        };
        let source_id = format!("{store_name}:{}", Uuid::now_v7());
        let blob_ref = store
            .spill_readable_json(
                &text,
                self.scope(),
                crate::blob::SpillSource::StoreValue,
                Some(&source_id),
            )
            .await
            .map_err(|e| format!("store write failed: {e}"))?;
        serde_json::to_value(&blob_ref).map_err(|e| e.to_string())
    }

    /// `value` as code reads it. Fails closed: a reference that can't be read
    /// is an error, not a value to hand to code.
    async fn read(
        &self,
        value: JsonValue,
        usage: &mut RehydrateUsage,
    ) -> Result<JsonValue, String> {
        if !is_spilled(&value) {
            return Ok(value);
        }
        let store = self
            .blob_store()
            .await
            .ok_or("store value is in blob storage, which is not configured")?;
        store
            .read_whole_json(&value, self.scope(), usage)
            .await
            .map_err(|e| format!("store read failed: {e}"))
    }

    async fn read_entry(
        &self,
        mut entry: StoreEntry,
        usage: &mut RehydrateUsage,
    ) -> Result<StoreEntry, String> {
        entry.value = self.read(entry.value, usage).await?;
        Ok(entry)
    }

    /// Stop holding on to a blob no entry refers to any more. Failing leaves it
    /// stored, which costs space but loses nothing.
    async fn release(&self, values: impl IntoIterator<Item = &JsonValue>) {
        for value in values.into_iter().filter(|value| is_spilled(value)) {
            let (Some(id), Some(store)) = (ref_id(value), self.blob_store().await) else {
                continue;
            };
            if let Err(e) = store.release_ref(id, self.scope()).await {
                tracing::warn!(blob_ref_id = %id, error = %e, "failed to release store value's blob");
            }
        }
    }

    /// Release a blob written for a value that didn't get stored.
    async fn release_unless_same(&self, stored: &JsonValue, value: &JsonValue) {
        if stored != value {
            self.release([stored]).await;
        }
    }
}

#[async_trait]
impl Store for SpillingStore {
    async fn ensure_store(&self, config: &StoreMapConfig) -> Result<(), String> {
        self.inner.ensure_store(config).await
    }

    async fn put(
        &self,
        store_name: &str,
        key: JsonValue,
        value: JsonValue,
        embedding: Option<Vec<f32>>,
        text_content: Option<String>,
    ) -> Result<StoreEntry, String> {
        let stored = self.stored_form(store_name, &value).await?;
        let written = self
            .inner
            .put(store_name, key, stored.clone(), embedding, text_content)
            .await;
        match written {
            Ok(mut entry) => {
                self.release(entry.replaced.iter()).await;
                entry.value = value;
                Ok(entry)
            }
            Err(e) => {
                self.release_unless_same(&stored, &value).await;
                Err(e)
            }
        }
    }

    async fn put_if_missing(
        &self,
        store_name: &str,
        key: JsonValue,
        value: JsonValue,
        embedding: Option<Vec<f32>>,
        text_content: Option<String>,
    ) -> Result<bool, String> {
        let stored = self.stored_form(store_name, &value).await?;
        let written = self
            .inner
            .put_if_missing(store_name, key, stored.clone(), embedding, text_content)
            .await;
        if !matches!(written, Ok(true)) {
            self.release_unless_same(&stored, &value).await;
        }
        written
    }

    async fn put_if(
        &self,
        store_name: &str,
        key: JsonValue,
        expected: JsonValue,
        value: JsonValue,
        embedding: Option<Vec<f32>>,
        text_content: Option<String>,
    ) -> Result<bool, String> {
        let stored = self.stored_form(store_name, &value).await?;
        let mut written = self
            .inner
            .put_if(
                store_name,
                key.clone(),
                expected.clone(),
                stored.clone(),
                embedding.clone(),
                text_content.clone(),
            )
            .await;
        // The entry may hold a reference to the expected value. Its reference
        // is unique to one write, so comparing and swapping that is as atomic
        // as comparing the value.
        if matches!(written, Ok(false))
            && let Some(current) = self.inner.get(store_name, &key).await?
            && is_spilled(&current.value)
        {
            let read = self
                .read(current.value.clone(), &mut RehydrateUsage::default())
                .await?;
            if super::stored_value_matches(Some(&read), &expected) {
                written = self
                    .inner
                    .put_if(
                        store_name,
                        key,
                        current.value.clone(),
                        stored.clone(),
                        embedding,
                        text_content,
                    )
                    .await;
                if matches!(written, Ok(true)) {
                    self.release([&current.value]).await;
                }
            }
        }
        if !matches!(written, Ok(true)) {
            self.release_unless_same(&stored, &value).await;
        }
        written
    }

    async fn increment(&self, store_name: &str, key: JsonValue, by: i64) -> Result<i64, String> {
        self.inner.increment(store_name, key, by).await
    }

    async fn get(&self, store_name: &str, key: &JsonValue) -> Result<Option<StoreEntry>, String> {
        match self.inner.get(store_name, key).await? {
            Some(entry) => Ok(Some(
                self.read_entry(entry, &mut RehydrateUsage::default())
                    .await?,
            )),
            None => Ok(None),
        }
    }

    async fn get_info(
        &self,
        store_name: &str,
        key: &JsonValue,
    ) -> Result<Option<StoreEntryInfo>, String> {
        self.inner.get_info(store_name, key).await
    }

    async fn delete(&self, store_name: &str, key: &JsonValue) -> Result<bool, String> {
        let spilled = self.inner.spilled_values(store_name, Some(key)).await?;
        let deleted = self.inner.delete(store_name, key).await?;
        if deleted {
            self.release(&spilled).await;
        }
        Ok(deleted)
    }

    async fn keys(&self, store_name: &str) -> Result<Vec<JsonValue>, String> {
        self.inner.keys(store_name).await
    }

    async fn vals(&self, store_name: &str) -> Result<Vec<JsonValue>, String> {
        let mut usage = RehydrateUsage::default();
        let mut vals = Vec::new();
        for value in self.inner.vals(store_name).await? {
            vals.push(self.read(value, &mut usage).await?);
        }
        Ok(vals)
    }

    async fn length(&self, store_name: &str) -> Result<usize, String> {
        self.inner.length(store_name).await
    }

    async fn first(&self, store_name: &str) -> Result<Option<StoreEntry>, String> {
        match self.inner.first(store_name).await? {
            Some(entry) => Ok(Some(
                self.read_entry(entry, &mut RehydrateUsage::default())
                    .await?,
            )),
            None => Ok(None),
        }
    }

    async fn last(&self, store_name: &str) -> Result<Option<StoreEntry>, String> {
        match self.inner.last(store_name).await? {
            Some(entry) => Ok(Some(
                self.read_entry(entry, &mut RehydrateUsage::default())
                    .await?,
            )),
            None => Ok(None),
        }
    }

    async fn list(
        &self,
        store_name: &str,
        options: ListOptions,
    ) -> Result<Vec<StoreEntry>, String> {
        let mut usage = RehydrateUsage::default();
        let mut entries = Vec::new();
        for entry in self.inner.list(store_name, options).await? {
            entries.push(self.read_entry(entry, &mut usage).await?);
        }
        Ok(entries)
    }

    async fn list_info(
        &self,
        store_name: &str,
        options: ListOptions,
    ) -> Result<Vec<StoreEntryInfo>, String> {
        self.inner.list_info(store_name, options).await
    }

    async fn clear(&self, store_name: &str) -> Result<(), String> {
        let spilled = self.inner.spilled_values(store_name, None).await?;
        self.inner.clear(store_name).await?;
        self.release(&spilled).await;
        Ok(())
    }

    async fn destroy(&self, store_name: &str) -> Result<(), String> {
        let spilled = self.inner.spilled_values(store_name, None).await?;
        self.inner.destroy(store_name).await?;
        self.release(&spilled).await;
        Ok(())
    }

    async fn put_many(&self, store_name: &str, entries: Vec<StoreWrite>) -> Result<usize, String> {
        let mut replaced = Vec::new();
        let mut written = Vec::new();
        for (key, value, embedding, text_content) in &entries {
            replaced.extend(self.inner.spilled_values(store_name, Some(key)).await?);
            let stored = self.stored_form(store_name, value).await?;
            written.push((key.clone(), stored, embedding.clone(), text_content.clone()));
        }
        let stored: Vec<JsonValue> = written.iter().map(|write| write.1.clone()).collect();
        match self.inner.put_many(store_name, written).await {
            Ok(count) => {
                self.release(&replaced).await;
                Ok(count)
            }
            Err(e) => {
                for (stored, (_, value, _, _)) in stored.iter().zip(&entries) {
                    self.release_unless_same(stored, value).await;
                }
                Err(e)
            }
        }
    }

    async fn search(
        &self,
        store_name: &str,
        query_text: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        options: SearchOptions,
    ) -> Result<Vec<SearchResult>, String> {
        let mut usage = RehydrateUsage::default();
        let mut results = Vec::new();
        for mut result in self
            .inner
            .search(store_name, query_text, query_embedding, options)
            .await?
        {
            result.entry = self.read_entry(result.entry, &mut usage).await?;
            results.push(result);
        }
        Ok(results)
    }

    async fn search_info(
        &self,
        store_name: &str,
        query_text: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        search_options: SearchOptions,
        list_options: ListOptions,
    ) -> Result<SearchInfoPage, String> {
        self.inner
            .search_info(
                store_name,
                query_text,
                query_embedding,
                search_options,
                list_options,
            )
            .await
    }

    async fn storage_bytes(&self) -> Result<i64, String> {
        self.inner.storage_bytes().await
    }

    fn storage_type(&self) -> &str {
        self.inner.storage_type()
    }

    async fn list_maps(&self) -> Result<Vec<StoreMapInfo>, String> {
        self.inner.list_maps().await
    }

    async fn spilled_values(
        &self,
        store_name: &str,
        key: Option<&JsonValue>,
    ) -> Result<Vec<JsonValue>, String> {
        self.inner.spilled_values(store_name, key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NAME: &str = "big-store";

    struct Fixture {
        store: SpillingStore,
        db: DatabasePool,
        _dir: tempfile::TempDir,
    }

    async fn fixture_with(blob: JsonValue) -> Fixture {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let conf: Val = serde_json::from_value(json!({
            "blob": blob,
            "file": {"storage": {"path": dir.path().to_str().unwrap()}},
        }))
        .unwrap();
        let db_arc = Arc::new(db.clone());
        let store = SpillingStore::new(
            Box::new(super::super::sqlite::SqliteStore::new(
                db_arc.clone(),
                data.org_id,
                data.env_id,
            )),
            conf,
            db_arc,
            data.org_id,
            data.env_id,
        );
        store
            .ensure_store(&StoreMapConfig {
                name: NAME.to_string(),
                embedding_provider: None,
                embedding_model: None,
                embedding_conf: None,
                embedding_field: None,
                embedding_dimensions: None,
                text_search: false,
                embedding_on_error: None,
                embed_fn: None,
                embed_batch_fn: None,
            })
            .await
            .unwrap();
        Fixture {
            store,
            db,
            _dir: dir,
        }
    }

    async fn fixture() -> Fixture {
        fixture_with(json!({"mode": "service"})).await
    }

    /// Over the inline limit, with `tag` telling one value from another.
    fn large(tag: &str) -> JsonValue {
        json!({"tag": tag, "text": "x".repeat(MAX_INLINE_HARD_LIMIT_BYTES + 1)})
    }

    impl Fixture {
        /// What the entry at `key` holds, without reading it from blob storage.
        async fn raw(&self, key: &JsonValue) -> JsonValue {
            self.store
                .inner
                .get(NAME, key)
                .await
                .unwrap()
                .unwrap()
                .value
        }

        async fn active_refs(&self) -> i64 {
            let DatabasePool::Sqlite(pool) = &self.db else {
                panic!("test_db should return SQLite");
            };
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM blob_ref WHERE source_kind = 'store_value' AND active = 1",
            )
            .fetch_one(pool)
            .await
            .unwrap()
        }

        async fn put(&self, key: &JsonValue, value: &JsonValue) -> StoreEntry {
            self.store
                .put(NAME, key.clone(), value.clone(), None, None)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn a_large_value_reads_back_through_every_read() {
        let fixture = fixture().await;
        let store = &fixture.store;
        let key = json!("k");
        let value = large("a");

        let entry = fixture.put(&key, &value).await;
        assert_eq!(entry.value, value);
        assert!(is_spilled(&fixture.raw(&key).await));

        let got = store.get(NAME, &key).await.unwrap().unwrap();
        assert_eq!(got.value, value);
        assert_eq!(store.vals(NAME).await.unwrap(), vec![value.clone()]);
        assert_eq!(store.first(NAME).await.unwrap().unwrap().value, value);
        assert_eq!(store.last(NAME).await.unwrap().unwrap().value, value);
        let listed = store.list(NAME, ListOptions::default()).await.unwrap();
        assert_eq!(listed[0].value, value);
    }

    #[tokio::test]
    async fn a_smaller_value_stays_inline() {
        let fixture = fixture().await;
        let key = json!("k");
        let value = json!({"text": "x".repeat(2 * 1024 * 1024)});
        fixture.put(&key, &value).await;
        assert_eq!(fixture.raw(&key).await, value);
        assert_eq!(fixture.active_refs().await, 0);
    }

    #[tokio::test]
    async fn a_decimal_keeps_its_digits() {
        let fixture = fixture().await;
        let key = json!("k");
        let dec = Val::Dec("19.90".parse().unwrap());
        let value = serde_json::from_str::<JsonValue>(&format!(
            "{{\"price\": {}, \"text\": {}}}",
            dec.to_exact_json(),
            json!("x".repeat(MAX_INLINE_HARD_LIMIT_BYTES + 1))
        ))
        .unwrap();
        fixture.put(&key, &value).await;
        let got = fixture.store.get(NAME, &key).await.unwrap().unwrap().value;
        let got: Val = serde_json::from_value(got).unwrap();
        assert_eq!(got.get("price"), Some(dec));
    }

    #[tokio::test]
    async fn the_entry_counts_the_blob_toward_the_quota() {
        let fixture = fixture().await;
        let key = json!("k");
        fixture.put(&key, &large("a")).await;
        let DatabasePool::Sqlite(pool) = &fixture.db else {
            panic!("test_db should return SQLite");
        };
        let size: i64 = sqlx::query_scalar("SELECT size FROM store_map_entry")
            .fetch_one(pool)
            .await
            .unwrap();
        assert!(size > MAX_INLINE_HARD_LIMIT_BYTES as i64, "{size}");
    }

    #[tokio::test]
    async fn overwriting_releases_the_old_blob() {
        let fixture = fixture().await;
        let key = json!("k");
        fixture.put(&key, &large("a")).await;
        assert_eq!(fixture.active_refs().await, 1);

        fixture.put(&key, &large("b")).await;
        assert_eq!(fixture.active_refs().await, 1);

        fixture.put(&key, &json!(1)).await;
        assert_eq!(fixture.active_refs().await, 0);
    }

    #[tokio::test]
    async fn writing_the_same_large_value_again_keeps_it_readable() {
        let fixture = fixture().await;
        let key = json!("k");
        let value = large("a");
        fixture.put(&key, &value).await;
        fixture.put(&key, &value).await;
        let got = fixture.store.get(NAME, &key).await.unwrap().unwrap();
        assert_eq!(got.value, value);
        assert_eq!(fixture.active_refs().await, 1);
    }

    #[tokio::test]
    async fn deleting_clearing_and_destroying_release_blobs() {
        let fixture = fixture().await;
        let store = &fixture.store;
        fixture.put(&json!("a"), &large("a")).await;
        fixture.put(&json!("b"), &large("b")).await;
        fixture.put(&json!("c"), &large("c")).await;

        assert!(store.delete(NAME, &json!("a")).await.unwrap());
        assert_eq!(fixture.active_refs().await, 2);

        assert!(!store.delete(NAME, &json!("a")).await.unwrap());
        assert_eq!(fixture.active_refs().await, 2);

        store.clear(NAME).await.unwrap();
        assert_eq!(fixture.active_refs().await, 0);

        fixture.put(&json!("d"), &large("d")).await;
        store.destroy(NAME).await.unwrap();
        assert_eq!(fixture.active_refs().await, 0);
    }

    #[tokio::test]
    async fn put_if_missing_keeps_the_entry_and_releases_what_it_wrote() {
        let fixture = fixture().await;
        let store = &fixture.store;
        let key = json!("k");
        let value = large("a");
        assert!(
            store
                .put_if_missing(NAME, key.clone(), value.clone(), None, None)
                .await
                .unwrap()
        );
        assert!(
            !store
                .put_if_missing(NAME, key.clone(), large("b"), None, None)
                .await
                .unwrap()
        );
        assert_eq!(fixture.active_refs().await, 1);
        let got = store.get(NAME, &key).await.unwrap().unwrap();
        assert_eq!(got.value, value);
    }

    #[tokio::test]
    async fn put_if_compares_the_value_not_the_reference() {
        let fixture = fixture().await;
        let store = &fixture.store;
        let key = json!("k");
        let (a, b, c) = (large("a"), large("b"), large("c"));
        fixture.put(&key, &a).await;

        // The wrong value: nothing changes, and what it wrote is released.
        assert!(
            !store
                .put_if(NAME, key.clone(), b.clone(), c.clone(), None, None)
                .await
                .unwrap()
        );
        assert_eq!(store.get(NAME, &key).await.unwrap().unwrap().value, a);
        assert_eq!(fixture.active_refs().await, 1);

        // The right value: replaced, and the old blob released.
        assert!(
            store
                .put_if(NAME, key.clone(), a.clone(), b.clone(), None, None)
                .await
                .unwrap()
        );
        assert_eq!(store.get(NAME, &key).await.unwrap().unwrap().value, b);
        assert_eq!(fixture.active_refs().await, 1);

        // A large value can give way to a small one and back.
        assert!(
            store
                .put_if(NAME, key.clone(), b.clone(), json!(1), None, None)
                .await
                .unwrap()
        );
        assert_eq!(fixture.active_refs().await, 0);
        assert!(
            store
                .put_if(NAME, key.clone(), json!(1), c.clone(), None, None)
                .await
                .unwrap()
        );
        assert_eq!(store.get(NAME, &key).await.unwrap().unwrap().value, c);
    }

    #[tokio::test]
    async fn a_value_that_looks_like_a_reference_reads_back_as_itself() {
        let fixture = fixture().await;
        let key = json!("k");
        let lookalike = serde_json::to_value(
            BlobRefVal {
                id: Uuid::now_v7(),
                object_id: Uuid::now_v7(),
                hash_alg: crate::blob::HASH_ALG_BLAKE3.to_string(),
                hash: "abc123".to_string(),
                size: 1,
                content_type: None,
                preview: None,
                encoding: crate::blob::ENCODING_JSON.to_string(),
            }
            .to_val(),
        )
        .unwrap();
        fixture.put(&key, &lookalike).await;
        let got = fixture.store.get(NAME, &key).await.unwrap().unwrap();
        assert_eq!(got.value, lookalike);
    }

    #[tokio::test]
    async fn one_read_shares_one_budget() {
        let fixture = fixture_with(json!({
            "mode": "service",
            "rehydrate": {"max-total-bytes": (MAX_INLINE_HARD_LIMIT_BYTES + 1024)},
        }))
        .await;
        let store = &fixture.store;
        fixture.put(&json!("a"), &large("a")).await;
        fixture.put(&json!("b"), &large("b")).await;

        // Each reads back alone, but not both at once.
        assert!(store.get(NAME, &json!("a")).await.unwrap().is_some());
        assert!(store.get(NAME, &json!("b")).await.unwrap().is_some());
        let error = store.list(NAME, ListOptions::default()).await.unwrap_err();
        assert!(error.contains("budget"), "{error}");
    }

    #[tokio::test]
    async fn a_value_too_large_to_read_back_is_refused_and_the_old_value_kept() {
        let fixture = fixture_with(json!({
            "mode": "service",
            "rehydrate": {"max-total-bytes": (MAX_INLINE_HARD_LIMIT_BYTES)},
        }))
        .await;
        let key = json!("k");
        fixture.put(&key, &json!(1)).await;
        let error = fixture
            .store
            .put(NAME, key.clone(), large("a"), None, None)
            .await
            .unwrap_err();
        assert!(error.contains("max-total-bytes"), "{error}");
        assert_eq!(fixture.raw(&key).await, json!(1));
    }

    #[tokio::test]
    async fn without_blob_storage_a_large_value_goes_to_the_backend() {
        let fixture = fixture_with(json!({"mode": "disabled"})).await;
        let key = json!("k");
        let value = large("a");
        fixture.put(&key, &value).await;
        assert_eq!(fixture.raw(&key).await, value);
    }

    #[tokio::test]
    async fn a_reference_without_blob_storage_is_an_error() {
        let fixture = fixture().await;
        let key = json!("k");
        fixture.put(&key, &large("a")).await;
        let stored = fixture.raw(&key).await;

        let without = fixture_with(json!({"mode": "disabled"})).await;
        without
            .store
            .inner
            .put(NAME, key.clone(), stored, None, None)
            .await
            .unwrap();
        assert!(without.store.get(NAME, &key).await.is_err());
    }
}

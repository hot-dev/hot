use ahash::AHashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone)]
struct CachedValue<V> {
    value: V,
    cached_at: Instant,
}

/// Small bounded in-memory cache for immutable or rarely-mutated DB entities
/// used on worker hot paths. Mutating code paths must explicitly invalidate.
pub struct EntityCache<K, V> {
    entries: Mutex<AHashMap<K, CachedValue<V>>>,
    max_entries: usize,
}

impl<K, V> EntityCache<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone,
{
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(AHashMap::new()),
            max_entries,
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        let cache = self.entries.lock().ok()?;
        cache.get(key).map(|entry| entry.value.clone())
    }

    pub fn insert(&self, key: K, value: V) {
        let Ok(mut cache) = self.entries.lock() else {
            return;
        };

        if cache.len() >= self.max_entries
            && !cache.contains_key(&key)
            && let Some((oldest_key, _)) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.cached_at)
                .map(|(key, entry)| (key.clone(), entry.cached_at))
        {
            cache.remove(&oldest_key);
        }

        cache.insert(
            key,
            CachedValue {
                value,
                cached_at: Instant::now(),
            },
        );
    }

    pub fn invalidate(&self, key: &K) {
        if let Ok(mut cache) = self.entries.lock() {
            cache.remove(key);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut cache) = self.entries.lock() {
            cache.clear();
        }
    }
}

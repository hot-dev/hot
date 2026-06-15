use async_trait::async_trait;

use crate::val::Val;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Generate a single embedding vector for the given text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String>;

    /// Batch-embed multiple texts for efficiency.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String>;

    /// Dimensionality of the vectors produced by this provider.
    fn dimensions(&self) -> u32;

    /// Model identifier (e.g. "bge-base-en-v1.5").
    fn model_name(&self) -> &str;

    /// Ensure the model is downloaded and ready. Idempotent.
    async fn ensure_ready(&self) -> Result<(), String>;

    fn provider_type(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Local embedding provider (fastembed / ONNX)
// ---------------------------------------------------------------------------

#[cfg(feature = "local-embeddings")]
pub mod local {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio::sync::OnceCell;

    pub struct LocalEmbeddingProvider {
        model_name: String,
        cache_dir: PathBuf,
        model: OnceCell<Arc<Mutex<fastembed::TextEmbedding>>>,
    }

    impl LocalEmbeddingProvider {
        pub fn new(model_name: &str, cache_dir: &str) -> Self {
            Self {
                model_name: model_name.to_string(),
                cache_dir: PathBuf::from(cache_dir),
                model: OnceCell::new(),
            }
        }

        async fn get_model(&self) -> Result<Arc<Mutex<fastembed::TextEmbedding>>, String> {
            self.model
                .get_or_try_init(|| async {
                    let model_name = self.model_name.clone();
                    let cache_dir = self.cache_dir.clone();

                    tokio::task::spawn_blocking(move || {
                        let model_type = match model_name.as_str() {
                            "bge-small-en-v1.5" => {
                                fastembed::EmbeddingModel::BGESmallENV15
                            }
                            "bge-base-en-v1.5" | "" => {
                                fastembed::EmbeddingModel::BGEBaseENV15
                            }
                            "bge-large-en-v1.5" => {
                                fastembed::EmbeddingModel::BGELargeENV15
                            }
                            other => {
                                return Err(format!(
                                    "Unknown local embedding model: {other}. \
                                     Supported: bge-small-en-v1.5, bge-base-en-v1.5, bge-large-en-v1.5"
                                ));
                            }
                        };

                        std::fs::create_dir_all(&cache_dir).map_err(|e| {
                            format!("Failed to create model cache dir: {e}")
                        })?;

                        let init = fastembed::InitOptions::new(model_type)
                            .with_cache_dir(cache_dir)
                            .with_show_download_progress(true);

                        let model = fastembed::TextEmbedding::try_new(init)
                            .map_err(|e| format!("Failed to load embedding model: {e}"))?;

                        Ok(Arc::new(Mutex::new(model)))
                    })
                    .await
                    .map_err(|e| format!("Embedding model init task failed: {e}"))?
                })
                .await
                .cloned()
        }
    }

    #[async_trait]
    impl EmbeddingProvider for LocalEmbeddingProvider {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
            let model = self.get_model().await?;
            let text = text.to_string();
            tokio::task::spawn_blocking(move || {
                let mut model = model
                    .lock()
                    .map_err(|e| format!("Embedding model lock poisoned: {e}"))?;
                let results = model
                    .embed(vec![text], None)
                    .map_err(|e| format!("Embedding failed: {e}"))?;
                results
                    .into_iter()
                    .next()
                    .ok_or_else(|| "No embedding returned".to_string())
            })
            .await
            .map_err(|e| format!("Embedding task failed: {e}"))?
        }

        async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
            let model = self.get_model().await?;
            let texts: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
            tokio::task::spawn_blocking(move || {
                let mut model = model
                    .lock()
                    .map_err(|e| format!("Embedding model lock poisoned: {e}"))?;
                model
                    .embed(texts, None)
                    .map_err(|e| format!("Batch embedding failed: {e}"))
            })
            .await
            .map_err(|e| format!("Batch embedding task failed: {e}"))?
        }

        fn dimensions(&self) -> u32 {
            match self.model_name.as_str() {
                "bge-small-en-v1.5" => 384,
                "bge-base-en-v1.5" | "" => 768,
                "bge-large-en-v1.5" => 1024,
                _ => 768,
            }
        }

        fn model_name(&self) -> &str {
            &self.model_name
        }

        async fn ensure_ready(&self) -> Result<(), String> {
            let _ = self.get_model().await?;
            Ok(())
        }

        fn provider_type(&self) -> &str {
            "local"
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::LazyLock;

        const TEST_MODEL_NAME: &str = "bge-small-en-v1.5";
        const TEST_MODEL_DIMENSIONS: usize = 384;

        static TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
            LazyLock::new(|| tokio::sync::Mutex::new(()));

        fn test_cache_dir() -> String {
            std::env::var("HOT_LOCAL_EMBEDDING_TEST_CACHE").unwrap_or_else(|_| {
                std::env::temp_dir()
                    .join("hot-local-embedding-tests")
                    .to_string_lossy()
                    .into_owned()
            })
        }

        fn test_provider() -> LocalEmbeddingProvider {
            LocalEmbeddingProvider::new(TEST_MODEL_NAME, &test_cache_dir())
        }

        fn assert_embedding_shape(embedding: &[f32]) {
            assert_eq!(embedding.len(), TEST_MODEL_DIMENSIONS);
            assert!(
                embedding.iter().all(|value| value.is_finite()),
                "embedding contained a non-finite value"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[ignore = "downloads and initializes a FastEmbed ONNX model"]
        async fn local_embedding_provider_embeds_single_and_batch() {
            let _guard = TEST_LOCK.lock().await;
            let provider = test_provider();

            assert_eq!(provider.provider_type(), "local");
            assert_eq!(provider.model_name(), TEST_MODEL_NAME);
            assert_eq!(provider.dimensions(), TEST_MODEL_DIMENSIONS as u32);

            provider.ensure_ready().await.unwrap();

            let single = provider
                .embed("hot local embedding smoke test")
                .await
                .unwrap();
            assert_embedding_shape(&single);

            let batch = provider
                .embed_batch(&[
                    "hot local embedding batch test one",
                    "hot local embedding batch test two",
                ])
                .await
                .unwrap();
            assert_eq!(batch.len(), 2);
            for embedding in batch {
                assert_embedding_shape(&embedding);
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore = "downloads and initializes a FastEmbed ONNX model"]
        async fn local_embedding_provider_handles_concurrent_embeds() {
            let _guard = TEST_LOCK.lock().await;
            let provider = Arc::new(test_provider());
            provider.ensure_ready().await.unwrap();

            let handles = (0..4)
                .map(|idx| {
                    let provider = Arc::clone(&provider);
                    tokio::spawn(async move {
                        let text = format!("hot local embedding concurrent test {idx}");
                        provider.embed(&text).await
                    })
                })
                .collect::<Vec<_>>();

            for handle in handles {
                let embedding = handle.await.unwrap().unwrap();
                assert_embedding_shape(&embedding);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

pub fn embedding_provider_from_config(conf: &Val) -> Option<Box<dyn EmbeddingProvider>> {
    let provider_str = conf
        .get("store.embedding.provider")
        .and_then(|v| match v {
            Val::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "local".to_string());

    let _model = conf
        .get("store.embedding.model")
        .and_then(|v| match v {
            Val::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "bge-base-en-v1.5".to_string());

    let _cache_dir = conf
        .get("store.models.path")
        .and_then(|v| match v {
            Val::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_else(|| ".hot/models".to_string());

    match provider_str.as_str() {
        #[cfg(feature = "local-embeddings")]
        "local" => Some(Box::new(local::LocalEmbeddingProvider::new(
            &_model,
            &_cache_dir,
        ))),
        #[cfg(not(feature = "local-embeddings"))]
        "local" => {
            tracing::warn!(
                "Local embeddings requested but 'local-embeddings' feature is not enabled. \
                 Embeddings will not be available."
            );
            None
        }
        other => {
            tracing::warn!(
                "Unknown embedding provider '{other}'. Embeddings will not be available."
            );
            None
        }
    }
}

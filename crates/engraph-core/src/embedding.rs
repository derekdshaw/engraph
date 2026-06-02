//! Pluggable embedding provider. Local-default behind the `embeddings`
//! feature flag; cloud providers slot in later via the same trait.
//!
//! Compile with `--features embeddings` to enable the fastembed-backed
//! implementation. Without the feature, only the trait is exposed and
//! `default_provider()` returns `None`.

use crate::Result;

pub trait EmbeddingProvider: Send + Sync {
    /// Embed a single text into a fixed-dimension vector.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Stable identifier for the model. Stored alongside vectors so a
    /// future model upgrade can detect stale rows.
    fn model_id(&self) -> &str;
    /// Vector dimension. Used for schema validation.
    fn dim(&self) -> usize;
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

#[cfg(feature = "embeddings")]
pub mod fastembed_provider {
    use super::{EmbeddingProvider, Result};
    use crate::Error;
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use std::sync::Mutex;

    pub struct FastEmbedProvider {
        inner: Mutex<TextEmbedding>,
        model_id: &'static str,
        dim: usize,
    }

    impl FastEmbedProvider {
        /// bge-small-en-v1.5 — 33M params, 384 dimensions, ~130MB on disk.
        pub fn bge_small_en() -> Result<Self> {
            let opts = InitOptions::new(EmbeddingModel::BGESmallENV15);
            let model = TextEmbedding::try_new(opts)
                .map_err(|e| Error::Config(format!("fastembed init: {e}")))?;
            Ok(Self {
                inner: Mutex::new(model),
                model_id: "bge-small-en-v1.5",
                dim: 384,
            })
        }
    }

    impl EmbeddingProvider for FastEmbedProvider {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let guard = self.inner.lock().expect("embedding model poisoned");
            let mut out = guard
                .embed(vec![text.to_string()], None)
                .map_err(|e| Error::Config(format!("fastembed embed: {e}")))?;
            out.pop()
                .ok_or_else(|| Error::Config("fastembed returned no vectors".into()))
        }
        fn model_id(&self) -> &str {
            self.model_id
        }
        fn dim(&self) -> usize {
            self.dim
        }
    }
}

#[cfg(feature = "embeddings")]
pub fn default_provider() -> Result<Box<dyn EmbeddingProvider>> {
    Ok(Box::new(
        fastembed_provider::FastEmbedProvider::bge_small_en()?,
    ))
}

#[cfg(not(feature = "embeddings"))]
pub fn default_provider() -> Result<Box<dyn EmbeddingProvider>> {
    Err(crate::Error::Config(
        "embeddings feature not enabled; rebuild with --features embeddings".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine(&a, &a) > 0.999);
        assert!(cosine(&a, &b).abs() < 0.001);
    }

    #[test]
    fn cosine_handles_mismatched_lengths() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }
}

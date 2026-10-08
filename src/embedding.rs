//! Bounded embedding access shared by memory retrieval and the code index.
//! Owns backend selection, fingerprints, caching, deadlines, and failure state;
//! it knows nothing about memory records, conversations, or repository storage.
use anyhow::{Result, ensure};
use builder_core::{
    config::{Config, EmbeddingBackend, ModelRole},
    memory::digest,
};
use builder_provider::OpenAiCompatible;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    time::Duration,
};

enum Embedding {
    Local(builder_embedding::LocalEmbedding),
    Remote(Box<OpenAiCompatible>),
}

pub struct EmbeddingRuntime {
    embedding: Option<Embedding>,
    fingerprint: String,
    query_prefix: String,
    document_prefix: String,
    embedding_failed: Cell<bool>,
    embedding_error: RefCell<Option<String>>,
    query_cache: RefCell<Option<(String, Vec<f32>)>>,
}

impl EmbeddingRuntime {
    pub fn lexical() -> Self {
        Self {
            embedding: None,
            fingerprint: String::new(),
            query_prefix: String::new(),
            document_prefix: String::new(),
            embedding_failed: Cell::new(false),
            embedding_error: RefCell::new(None),
            query_cache: RefCell::new(None),
        }
    }
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut runtime = Self::lexical();
        match config.memory.embedding_backend {
            EmbeddingBackend::Local => {
                runtime.fingerprint = digest(builder_embedding::FINGERPRINT.as_bytes());
                runtime.embedding = Some(Embedding::Local(builder_embedding::LocalEmbedding::new(
                    config.memory.local_model_dir.clone(),
                )));
                return Ok(runtime);
            }
            EmbeddingBackend::Lexical => return Ok(runtime),
            EmbeddingBackend::Remote => {}
        }
        let selected = if let Some(name) = &config.memory.embedding_profile {
            Some(config.profile(Some(name))?.1)
        } else {
            let candidates = config
                .profiles
                .values()
                .filter(|p| p.roles.contains(&ModelRole::Embed))
                .collect::<Vec<_>>();
            if candidates.len() == 1 {
                Some(candidates[0].clone())
            } else {
                None
            }
        };
        if let Some(profile) = selected {
            ensure!(
                profile.roles.contains(&ModelRole::Embed),
                "Memory embedding profile must have the embed role"
            );
            runtime.fingerprint = digest(
                serde_json::to_string(&json!([
                    profile.base_url,
                    profile.model,
                    config.memory.query_prefix,
                    config.memory.document_prefix,
                    config.memory.embedding_revision
                ]))?
                .as_bytes(),
            );
            runtime.embedding = Some(Embedding::Remote(Box::new(OpenAiCompatible::new(profile)?)));
            runtime.query_prefix = config.memory.query_prefix.clone();
            runtime.document_prefix = config.memory.document_prefix.clone();
        }
        Ok(runtime)
    }
    pub fn reset(&self) {
        self.embedding_failed.set(false);
        self.embedding_error.borrow_mut().take();
        self.query_cache.borrow_mut().take();
    }
    pub async fn embed(&self, text: &str, query: bool) -> Option<Vec<f32>> {
        if query
            && let Some((cached, vector)) = &*self.query_cache.borrow()
            && cached == text
        {
            return Some(vector.clone());
        }
        if self.embedding_failed.get() {
            return None;
        }
        let provider = self.embedding.as_ref()?;
        let prefix = if query {
            &self.query_prefix
        } else {
            &self.document_prefix
        };
        let budget = if matches!(provider, Embedding::Local(_)) {
            15
        } else {
            3
        };
        match tokio::time::timeout(Duration::from_secs(budget), async {
            match provider {
                Embedding::Local(local) => local.embed(text).await,
                Embedding::Remote(remote) => remote.embed(&format!("{prefix}{text}")).await,
            }
        })
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "Embedding deadline exceeded; using lexical retrieval"
            ))
        }) {
            Ok(vector) => {
                if query {
                    *self.query_cache.borrow_mut() = Some((text.into(), vector.clone()));
                }
                Some(vector)
            }
            Err(error) => {
                *self.embedding_error.borrow_mut() = Some(error.to_string());
                self.embedding_failed.set(true);
                None
            }
        }
    }
    pub async fn embed_batch(&self, texts: &[String], timeout_secs: u64) -> Option<Vec<Vec<f32>>> {
        if self.embedding_failed.get() || texts.is_empty() {
            return None;
        }
        let provider = self.embedding.as_ref()?;
        let result = tokio::time::timeout(Duration::from_secs(timeout_secs.clamp(1, 600)), async {
            match provider {
                Embedding::Local(local) => local.embed_batch(texts.to_vec()).await,
                Embedding::Remote(remote) => {
                    let mut vectors = Vec::with_capacity(texts.len());
                    for text in texts {
                        vectors.push(
                            remote
                                .embed(&format!("{}{}", self.document_prefix, text))
                                .await?,
                        );
                    }
                    Ok(vectors)
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Code embedding batch deadline exceeded")));
        match result {
            Ok(vectors) if vectors.len() == texts.len() => Some(vectors),
            Ok(_) => {
                *self.embedding_error.borrow_mut() =
                    Some("Embedding provider returned an unexpected batch".into());
                self.embedding_failed.set(true);
                None
            }
            Err(error) => {
                *self.embedding_error.borrow_mut() = Some(error.to_string());
                self.embedding_failed.set(true);
                None
            }
        }
    }
    pub fn fingerprint(&self) -> Option<&str> {
        (!self.fingerprint.is_empty()).then_some(self.fingerprint.as_str())
    }
    pub fn error(&self) -> Option<String> {
        self.embedding_error.borrow().clone()
    }
    pub fn is_available(&self) -> bool {
        self.embedding.is_some() && !self.embedding_failed.get()
    }

    pub fn failed(&self) -> bool {
        self.embedding_failed.get()
    }
}

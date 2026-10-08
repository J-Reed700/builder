//! Source-backed memory policy. Derived notes never override instructions or outcomes.
mod evidence;
mod extraction;
mod retrieval;
mod tools;

use crate::embedding::EmbeddingRuntime;
use anyhow::Result;
use builder_core::config::Config;
use builder_tools::Workspace;
use std::cell::Cell;

pub use tools::{definitions, is_memory};

pub struct MemoryRuntime {
    embeddings: EmbeddingRuntime,
    extraction_attempt: Cell<i64>,
}

impl MemoryRuntime {
    pub fn lexical() -> Self {
        Self {
            embeddings: EmbeddingRuntime::lexical(),
            extraction_attempt: Cell::new(0),
        }
    }

    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        if !config.memory.enabled {
            return Ok(None);
        }
        Ok(Some(Self {
            embeddings: EmbeddingRuntime::from_config(config)?,
            extraction_attempt: Cell::new(0),
        }))
    }

    /// The shared embedding service; code retrieval needs no memory policy.
    pub fn embeddings(&self) -> &EmbeddingRuntime {
        &self.embeddings
    }

    pub fn scope(workspace: &Workspace) -> String {
        workspace.root().to_string_lossy().into_owned()
    }

    pub fn reset_network(&self) {
        self.extraction_attempt.set(0);
        self.embeddings.reset();
    }
}

fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    let mut end = max.saturating_sub(40);
    while !text.is_char_boundary(end) {
        end -= 1
    }
    format!("{} [excerpt; original retained]", &text[..end])
}

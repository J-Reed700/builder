//! Offline CPU embeddings. Only explicit installation performs network I/O.
use anyhow::{Context, Result, ensure};
use builder_core::memory::{digest, validate_vector};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use futures_util::StreamExt;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

pub const MODEL: &str = "all-MiniLM-L6-v2";
pub const REVISION: &str = "5f1b8cd78bc4fb444dd171e59b18f3a3af89a079";
pub const DIMENSIONS: usize = 384;
pub const FINGERPRINT: &str =
    "local:minilm-l6-v2:5f1b8cd78bc4fb444dd171e59b18f3a3af89a079:mean:256:384:v1";
const FILES: &[(&str, u64, &str)] = &[
    (
        "model.onnx",
        90387630,
        "bbd7b466f6d58e646fdc2bd5fd67b2f5e93c0b687011bd4548c420f7bd46f0c5",
    ),
    (
        "config.json",
        650,
        "1b4d8e2a3988377ed8b519a31d8d31025a25f1c5f8606998e8014111438efcd7",
    ),
    (
        "tokenizer.json",
        711661,
        "da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0",
    ),
    (
        "special_tokens_map.json",
        695,
        "5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a",
    ),
    (
        "tokenizer_config.json",
        1433,
        "bd2e06a5b20fd1b13ca988bedc8763d332d242381b4fbc98f8fead4524158f79",
    ),
];
fn checked_file(root: &Path, name: &str) -> Result<Vec<u8>> {
    let (_, size, hash) = FILES
        .iter()
        .find(|(file, _, _)| *file == name)
        .context("Unknown model asset")?;
    let file = std::fs::File::open(root.join(name))
        .context("Local embedding model missing; open /memory and choose local setup")?;
    ensure!(
        file.metadata()?.len() == *size,
        "Local embedding asset size mismatch: {name}; run local setup again"
    );
    let mut bytes = Vec::new();
    file.take(size + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == *size && digest(&bytes) == *hash,
        "Local embedding asset checksum mismatch: {name}; run local setup again"
    );
    Ok(bytes)
}

/// Download only pinned public model assets, without using chat/API credentials.
/// Every file is validated before atomic replacement. Cancellation removes temp files.
pub async fn install(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?;
    for (name, size, hash) in FILES {
        if checked_file(root, name).is_ok() {
            continue;
        }
        let url = format!(
            "https://huggingface.co/Qdrant/all-MiniLM-L6-v2-onnx/resolve/{REVISION}/{name}"
        );
        let response = client.get(url).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            ensure!(
                bytes.len() + chunk.len() <= *size as usize,
                "Model download exceeds expected size"
            );
            bytes.extend_from_slice(&chunk);
        }
        ensure!(
            bytes.len() == *size as usize && digest(&bytes) == *hash,
            "Model download failed checksum validation: {name}"
        );
        let mut temp = tempfile::NamedTempFile::new_in(root)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(root.join(name))?;
    }
    Ok(())
}

pub struct LocalEmbedding {
    root: Option<PathBuf>,
    // The owned guard stays with any in-flight blocking inference on cancellation.
    // Subsequent callers cannot queue additional model loads/inferences behind it.
    model: Arc<Mutex<Option<TextEmbedding>>>,
}
impl LocalEmbedding {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            model: Arc::new(Mutex::new(None)),
        }
    }
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        ensure!(
            !text.is_empty() && text.len() <= 8192,
            "Embedding input must be 1–8192 bytes"
        );
        let root = self
            .root
            .clone()
            .context("Local embeddings are not installed; open /memory and choose local setup")?;
        let mut guard = self.model.clone().lock_owned().await;
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || -> Result<Vec<f32>> {
            if guard.is_none() {
                let tokenizer = TokenizerFiles {
                    tokenizer_file: checked_file(&root, "tokenizer.json")?,
                    config_file: checked_file(&root, "config.json")?,
                    special_tokens_map_file: checked_file(&root, "special_tokens_map.json")?,
                    tokenizer_config_file: checked_file(&root, "tokenizer_config.json")?,
                };
                let model =
                    UserDefinedEmbeddingModel::new(checked_file(&root, "model.onnx")?, tokenizer)
                        .with_pooling(Pooling::Mean);
                *guard = Some(TextEmbedding::try_new_from_user_defined(
                    model,
                    InitOptionsUserDefined::new()
                        .with_max_length(256)
                        .with_intra_threads(2),
                )?);
            }
            let mut vectors = guard
                .as_mut()
                .context("Local model unavailable")?
                .embed(vec![text], Some(1))?;
            ensure!(
                vectors.len() == 1,
                "Local model returned an unexpected batch"
            );
            let vector = vectors.remove(0);
            validate_vector(&vector)?;
            ensure!(
                vector.len() == DIMENSIONS,
                "Local embedding dimensions changed"
            );
            Ok(vector)
        })
        .await
        .context("Local embedding worker failed")?
    }
}

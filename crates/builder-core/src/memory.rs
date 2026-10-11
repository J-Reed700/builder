//! Versioned derived memory and vector math. Original records remain authoritative.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub session: String,
    pub seq: i64,
    pub call_id: String,
    pub path: String,
    pub hash: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Finding,
    Preference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub key: String,
    pub revision: i64,
    pub kind: MemoryKind,
    pub text: String,
    pub evidence: Vec<Evidence>,
    pub origin_session: String,
    pub origin_seq: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub next_action: String,
    pub questions: Vec<String>,
    pub source_seq: i64,
}

pub fn validate_vector(vector: &[f32]) -> Result<()> {
    ensure!(
        !vector.is_empty() && vector.len() <= 8192 && vector.iter().all(|f| f.is_finite()),
        "Invalid embedding dimensions or values"
    );
    ensure!(
        vector.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() > 0.0,
        "Embedding has zero norm"
    );
    Ok(())
}
pub fn cosine(a: &[f32], b: &[f32]) -> Result<f64> {
    validate_vector(a)?;
    validate_vector(b)?;
    ensure!(a.len() == b.len(), "Embedding dimension mismatch");
    let dot = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum::<f64>();
    let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    Ok(dot / (norm(a) * norm(b)))
}

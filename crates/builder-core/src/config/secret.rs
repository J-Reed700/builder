use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// A literal credential or a reference resolved only when constructing transport.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Secret {
    Literal(String),
    Environment { env: String },
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}
impl Secret {
    pub fn resolve(&self) -> Result<String> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Environment { env } => {
                ensure!(
                    !env.is_empty(),
                    "Credential environment variable cannot be empty"
                );
                std::env::var(env)
                    .with_context(|| format!("Set environment variable {env} for this profile"))
            }
        }
    }
    pub fn redacted(&self) -> Self {
        match self {
            Self::Literal(_) => Self::Literal("[redacted]".into()),
            Self::Environment { .. } => self.clone(),
        }
    }
    /// Support the local environment forms used by Continue and OpenCode.
    pub fn imported(value: String) -> Result<Self> {
        let trimmed = value.trim();
        let env = trimmed
            .strip_prefix("{env:")
            .and_then(|s| s.strip_suffix('}'))
            .or_else(|| {
                trimmed
                    .strip_prefix("${{")
                    .and_then(|s| s.strip_suffix("}}"))
                    .map(str::trim)
                    .and_then(|s| s.strip_prefix("secrets."))
            });
        if let Some(env) = env {
            ensure!(
                !env.is_empty() && env.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "Invalid credential environment reference"
            );
            return Ok(Self::Environment { env: env.into() });
        }
        ensure!(
            !value.contains("${{") && !value.contains("{env:") && !value.contains("{file:"),
            "Unsupported credential template; use a whole-value environment reference or a literal"
        );
        Ok(Self::Literal(value))
    }
}

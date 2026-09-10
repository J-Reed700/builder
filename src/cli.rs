use builder::agent::ApprovalMode;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "builder",
    version,
    about = "A terminal coding agent for OpenAI-compatible model servers."
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        env = "BUILDER_HOME",
        help = "Configuration and session storage directory"
    )]
    pub home: Option<PathBuf>,
    #[arg(short, long, global = true, help = "Endpoint profile")]
    pub profile: Option<String>,
    /// Override a pipeline setting for this invocation; repeat for multiple settings.
    #[arg(long = "pipeline", global = true, value_name = "KEY=VALUE")]
    pub pipeline: Vec<String>,
    #[arg(short = 'C', long, global = true, default_value = ".")]
    pub workspace: PathBuf,
    #[arg(long, global = true, value_enum, default_value = "ask")]
    pub approval: Approval,
    /// Automatically approve file edits and shell commands (same as --approval trust).
    #[arg(long = "auto", global = true, conflicts_with = "approval")]
    pub auto: bool,
    /// Use a simple line prompt for terminals without cursor/paste support.
    #[arg(long, global = true)]
    pub plain: bool,
    /// Override the round budget from /settings for this invocation.
    #[arg(long, global=true, value_parser=clap::value_parser!(u16).range(1..=1000))]
    pub max_rounds: Option<u16>,
    #[command(subcommand)]
    pub command: Option<Command>,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Approval {
    Ask,
    ReadOnly,
    Trust,
}
impl From<Approval> for ApprovalMode {
    fn from(value: Approval) -> Self {
        match value {
            Approval::Ask => Self::Ask,
            Approval::ReadOnly => Self::ReadOnly,
            Approval::Trust => Self::Trust,
        }
    }
}
#[derive(Subcommand)]
pub enum Command {
    /// Serve this workspace for the Docker/browser remote-control interface.
    Remote {
        #[arg(long, default_value = "127.0.0.1:7432")]
        listen: std::net::SocketAddr,
        /// Exact browser origin, e.g. https://builder.example.com (no trailing slash).
        #[arg(long)]
        origin: Option<String>,
    },
    /// Start interactive chat (also the default command).
    Chat { prompt: Option<String> },
    /// Run one task. Use '-' to read the prompt from stdin.
    Run {
        prompt: String,
        #[arg(long)]
        session: Option<String>,
    },
    /// Reopen a saved session and retry any unfinished turn.
    Resume { session: Option<String> },
    /// List durable sessions.
    Sessions,
    /// Export a complete conversation as JSON or Markdown.
    Export {
        session: String,
        #[arg(long)]
        json: bool,
    },
    /// Manage local and hosted endpoint profiles.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Inspect, enable, update, or forget persistent memory.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Check configuration, storage, and endpoint model discovery.
    Doctor,
    /// List models advertised by the active endpoint.
    Models,
}
#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Inspect or configure the selected profile's research pipeline.
    Pipeline {
        #[command(subcommand)]
        command: PipelineCommand,
    },
    /// Create a default config if one does not exist.
    Init,
    /// Import custom endpoint models from Continue YAML or OpenCode JSON/JSONC.
    Import {
        path: PathBuf,
        #[arg(long, value_enum)]
        format: ImportFormat,
        /// Replace profiles with matching names.
        #[arg(long)]
        replace: bool,
        /// Select the imported chat model as the default.
        #[arg(long)]
        activate: bool,
    },
    /// Update the selected profile's server model ID, preserving all other settings.
    Model { model: String },
    /// Print configuration with API keys and header values redacted.
    Show,
    /// Add or replace a profile. URL includes /v1 if required by your server.
    Add {
        name: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        api_key_env: Option<String>,
        #[arg(long)]
        no_stream: bool,
        #[arg(long)]
        no_tools: bool,
        #[arg(long, default_value_t = 32768)]
        context_tokens: usize,
        #[arg(long, default_value_t = 4096)]
        max_output_tokens: usize,
    },
    /// Choose the default endpoint profile.
    Use { name: String },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ImportFormat {
    Continue,
    Opencode,
}
impl From<ImportFormat> for builder_core::config::import::Format {
    fn from(value: ImportFormat) -> Self {
        match value {
            ImportFormat::Continue => Self::Continue,
            ImportFormat::Opencode => Self::OpenCode,
        }
    }
}

impl Cli {
    pub fn approval_mode(&self) -> ApprovalMode {
        if self.auto {
            ApprovalMode::Trust
        } else {
            self.approval.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_global_and_maps_to_trust_without_changing_the_default() {
        assert_eq!(
            Cli::try_parse_from(["builder"]).unwrap().approval_mode(),
            ApprovalMode::Ask
        );
        for args in [
            vec!["builder", "--auto"],
            vec!["builder", "run", "hello", "--auto"],
            vec!["builder", "resume", "--auto"],
        ] {
            assert_eq!(
                Cli::try_parse_from(args).unwrap().approval_mode(),
                ApprovalMode::Trust
            );
        }
        assert!(Cli::try_parse_from(["builder", "--auto", "--approval", "read-only"]).is_err());
    }
}

#[derive(Subcommand)]
pub enum MemoryCommand {
    /// Enable local memory; download the local model once, or explicitly opt into a remote profile.
    Enable {
        #[arg(long, conflicts_with_all = ["embedding_profile", "lexical"])]
        local: bool,
        #[arg(long, conflicts_with = "embedding_profile")]
        lexical: bool,
        #[arg(long)]
        embedding_profile: Option<String>,
        #[arg(long)]
        query_prefix: Option<String>,
        #[arg(long)]
        document_prefix: Option<String>,
        #[arg(long)]
        embedding_revision: Option<String>,
    },
    Disable,
    Status,
    List {
        #[arg(long)]
        user: bool,
    },
    Get {
        key: String,
        #[arg(long)]
        revision: Option<i64>,
        #[arg(long)]
        user: bool,
    },
    Search {
        query: String,
    },
    /// Save an explicit user preference. Use the same key to revise it.
    Remember {
        key: String,
        text: String,
    },
    /// Remove a memory from retrieval, retaining audit history.
    Forget {
        key: String,
        #[arg(long)]
        user: bool,
    },
    Task {
        session: String,
    },
    /// Index up to four pending findings; safe to repeat or interrupt.
    Index,
}

#[derive(Subcommand)]
pub enum PipelineCommand {
    /// Show all saved settings and effective enabled operations (applies --pipeline overrides).
    Show,
    /// Persist one or more KEY=VALUE settings atomically for the selected profile.
    Set {
        #[arg(required=true, num_args=1.., value_name="KEY=VALUE")]
        settings: Vec<String>,
    },
    /// Restore the selected profile's pipeline defaults.
    Reset,
}

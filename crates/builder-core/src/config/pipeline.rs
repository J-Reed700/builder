//! Profile-local research policy. CLI updates and transient overrides share the
//! same typed decoder and validation; unknown keys never silently do nothing.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineSettings {
    pub enabled: bool,
    pub guidance: bool,
    pub planning: bool,
    pub observations: bool,
    pub symbols: bool,
    pub semantic: bool,
    pub hypotheses: bool,
    pub verification: bool,
    pub candidates: bool,
    pub review: bool,
    pub completion_gate: bool,
    pub procedures: bool,
    pub auto_recall: bool,
    pub history: bool,
    pub analysis: bool,
    pub phase_routing: bool,
    pub code_index: bool,
    pub code_index_background: bool,
    pub code_index_watch: bool,
    pub code_index_semantic: bool,
    pub code_index_auto_context: bool,
    pub code_index_telemetry: bool,
    pub code_history: bool,
    pub todos: bool,
    pub subagents: bool,
    pub max_rounds: usize,
    pub progress_check_calls: usize,
    pub progress_recovery_rounds: usize,
    pub failure_check_calls: usize,
    pub failure_recovery_rounds: usize,
    pub identical_shell_calls: usize,
    pub tool_calls_per_response: usize,
    pub parallel_tools: usize,
    pub subagent_parallel: usize,
    pub subagent_rounds: usize,
    pub candidate_attempts: usize,
    pub completion_retries: usize,
    pub analysis_artifacts: usize,
    pub analysis_timeout_secs: u64,
    pub analysis_output_tokens: usize,
    pub command_timeout_secs: u64,
    pub semantic_timeout_secs: u64,
    pub diagnostic_wait_secs: u64,
    pub snapshot_max_files: usize,
    pub snapshot_max_file_bytes: usize,
    pub snapshot_max_bytes: usize,
    pub history_page_bytes: usize,
    pub history_search_results: usize,
    pub procedure_results: usize,
    pub retrieval_records: usize,
    pub code_index_max_files: usize,
    pub code_index_max_file_bytes: usize,
    pub code_index_max_bytes: usize,
    pub code_index_max_chunks: usize,
    pub code_index_refresh_secs: u64,
    pub code_index_debounce_ms: u64,
    pub code_index_embedding_timeout_secs: u64,
    pub code_index_embedding_batch: usize,
    pub code_search_results: usize,
    pub code_index_lexical_candidates: usize,
    pub code_index_exact_candidates: usize,
    pub code_index_dense_candidates: usize,
    pub code_index_history_results: usize,
    pub code_index_chunks_per_file: usize,
    pub code_index_auto_candidates: usize,
    pub code_index_auto_files: usize,
    pub code_index_min_similarity_percent: usize,
    pub code_index_graph_hops: usize,
    pub code_index_graph_symbols: usize,
    pub code_index_graph_candidates: usize,
    pub code_history_commits: usize,
    pub code_history_timeout_secs: u64,
    pub memory_min_similarity_percent: usize,
    pub memory_min_margin_percent: usize,
}
impl Default for PipelineSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            guidance: true,
            planning: true,
            observations: true,
            symbols: true,
            semantic: true,
            hypotheses: true,
            verification: true,
            candidates: true,
            review: true,
            completion_gate: true,
            procedures: true,
            auto_recall: true,
            history: true,
            analysis: true,
            phase_routing: true,
            code_index: true,
            code_index_background: true,
            code_index_watch: true,
            code_index_semantic: true,
            code_index_auto_context: true,
            code_index_telemetry: true,
            code_history: true,
            todos: true,
            subagents: true,
            max_rounds: 100,
            progress_check_calls: 12,
            progress_recovery_rounds: 88,
            failure_check_calls: 3,
            failure_recovery_rounds: 3,
            identical_shell_calls: 3,
            tool_calls_per_response: 128,
            parallel_tools: 8,
            subagent_parallel: 3,
            subagent_rounds: 30,
            candidate_attempts: 3,
            completion_retries: 2,
            analysis_artifacts: 4,
            analysis_timeout_secs: 45,
            analysis_output_tokens: 2048,
            command_timeout_secs: 120,
            semantic_timeout_secs: 20,
            diagnostic_wait_secs: 5,
            snapshot_max_files: 20000,
            snapshot_max_file_bytes: 2 * 1024 * 1024,
            snapshot_max_bytes: 64 * 1024 * 1024,
            history_page_bytes: 8000,
            history_search_results: 16,
            procedure_results: 4,
            retrieval_records: 256,
            code_index_max_files: 20_000,
            code_index_max_file_bytes: 2 * 1024 * 1024,
            code_index_max_bytes: 64 * 1024 * 1024,
            code_index_max_chunks: 100_000,
            code_index_refresh_secs: 30,
            code_index_debounce_ms: 500,
            code_index_embedding_timeout_secs: 60,
            code_index_embedding_batch: 16,
            code_search_results: 10,
            code_index_lexical_candidates: 100,
            code_index_exact_candidates: 100,
            code_index_dense_candidates: 100,
            code_index_history_results: 20,
            code_index_chunks_per_file: 2,
            code_index_auto_candidates: 40,
            code_index_auto_files: 4,
            code_index_min_similarity_percent: 25,
            code_index_graph_hops: 2,
            code_index_graph_symbols: 16,
            code_index_graph_candidates: 80,
            code_history_commits: 500,
            code_history_timeout_secs: 5,
            memory_min_similarity_percent: 25,
            memory_min_margin_percent: 3,
        }
    }
}
impl PipelineSettings {
    pub fn validate(&self) -> Result<()> {
        for (name, value, min, max) in [
            ("max_rounds", self.max_rounds, 1, 1000),
            ("progress_check_calls", self.progress_check_calls, 1, 1000),
            (
                "progress_recovery_rounds",
                self.progress_recovery_rounds,
                1,
                1000,
            ),
            ("failure_check_calls", self.failure_check_calls, 1, 100),
            (
                "failure_recovery_rounds",
                self.failure_recovery_rounds,
                1,
                100,
            ),
            ("identical_shell_calls", self.identical_shell_calls, 1, 100),
            (
                "tool_calls_per_response",
                self.tool_calls_per_response,
                1,
                128,
            ),
            ("parallel_tools", self.parallel_tools, 1, 32),
            ("subagent_parallel", self.subagent_parallel, 1, 8),
            ("subagent_rounds", self.subagent_rounds, 1, 200),
            ("candidate_attempts", self.candidate_attempts, 1, 20),
            ("completion_retries", self.completion_retries, 0, 8),
            ("analysis_artifacts", self.analysis_artifacts, 1, 4),
            (
                "analysis_output_tokens",
                self.analysis_output_tokens,
                256,
                8192,
            ),
            ("snapshot_max_files", self.snapshot_max_files, 1, 20000),
            (
                "snapshot_max_file_bytes",
                self.snapshot_max_file_bytes,
                1,
                2 * 1024 * 1024,
            ),
            (
                "snapshot_max_bytes",
                self.snapshot_max_bytes,
                1,
                64 * 1024 * 1024,
            ),
            ("history_page_bytes", self.history_page_bytes, 256, 8000),
            ("history_search_results", self.history_search_results, 1, 16),
            ("procedure_results", self.procedure_results, 1, 4),
            ("retrieval_records", self.retrieval_records, 1, 256),
            ("code_index_max_files", self.code_index_max_files, 1, 50_000),
            (
                "code_index_max_file_bytes",
                self.code_index_max_file_bytes,
                1024,
                2 * 1024 * 1024,
            ),
            (
                "code_index_max_bytes",
                self.code_index_max_bytes,
                1024,
                256 * 1024 * 1024,
            ),
            (
                "code_index_max_chunks",
                self.code_index_max_chunks,
                1,
                100_000,
            ),
            (
                "code_index_embedding_batch",
                self.code_index_embedding_batch,
                1,
                128,
            ),
            ("code_search_results", self.code_search_results, 1, 20),
            (
                "code_index_lexical_candidates",
                self.code_index_lexical_candidates,
                1,
                500,
            ),
            (
                "code_index_exact_candidates",
                self.code_index_exact_candidates,
                1,
                500,
            ),
            (
                "code_index_dense_candidates",
                self.code_index_dense_candidates,
                1,
                500,
            ),
            (
                "code_index_history_results",
                self.code_index_history_results,
                1,
                100,
            ),
            (
                "code_index_chunks_per_file",
                self.code_index_chunks_per_file,
                1,
                8,
            ),
            (
                "code_index_auto_candidates",
                self.code_index_auto_candidates,
                1,
                200,
            ),
            ("code_index_auto_files", self.code_index_auto_files, 1, 12),
            (
                "code_index_min_similarity_percent",
                self.code_index_min_similarity_percent,
                0,
                100,
            ),
            ("code_index_graph_hops", self.code_index_graph_hops, 0, 3),
            (
                "code_index_graph_symbols",
                self.code_index_graph_symbols,
                1,
                32,
            ),
            (
                "code_index_graph_candidates",
                self.code_index_graph_candidates,
                1,
                200,
            ),
            ("code_history_commits", self.code_history_commits, 1, 5000),
            (
                "memory_min_similarity_percent",
                self.memory_min_similarity_percent,
                0,
                100,
            ),
            (
                "memory_min_margin_percent",
                self.memory_min_margin_percent,
                0,
                100,
            ),
        ] {
            ensure!(
                (min..=max).contains(&value),
                "pipeline.{name} must be between {min} and {max}"
            );
        }
        for (name, value) in [
            ("analysis_timeout_secs", self.analysis_timeout_secs),
            ("command_timeout_secs", self.command_timeout_secs),
            ("semantic_timeout_secs", self.semantic_timeout_secs),
            ("diagnostic_wait_secs", self.diagnostic_wait_secs),
        ] {
            ensure!(
                (1..=120).contains(&value),
                "pipeline.{name} must be between 1 and 120"
            );
        }
        ensure!(
            (1..=3600).contains(&self.code_index_refresh_secs),
            "pipeline.code_index_refresh_secs must be between 1 and 3600"
        );
        ensure!(
            (50..=10_000).contains(&self.code_index_debounce_ms),
            "pipeline.code_index_debounce_ms must be between 50 and 10000"
        );
        ensure!(
            (1..=600).contains(&self.code_index_embedding_timeout_secs),
            "pipeline.code_index_embedding_timeout_secs must be between 1 and 600"
        );
        ensure!(
            (1..=30).contains(&self.code_history_timeout_secs),
            "pipeline.code_history_timeout_secs must be between 1 and 30"
        );
        Ok(())
    }

    pub fn max_no_progress_calls(&self) -> usize {
        self.progress_check_calls
            .saturating_add(self.progress_recovery_rounds)
    }
    /// Apply a whole batch atomically. Hyphens and underscores are interchangeable
    /// in CLI keys; persisted TOML uses underscores. Values are booleans/integers.
    pub fn updated(&self, assignments: &[String]) -> Result<Self> {
        let mut value = serde_json::to_value(self)?;
        for assignment in assignments {
            let (key, text) = assignment
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("Expected pipeline KEY=VALUE"))?;
            let key = key.replace('-', "_");
            let slot = value.get_mut(&key).ok_or_else(|| {
                anyhow::anyhow!(
                    "Unknown pipeline setting '{key}'; use config pipeline show to list settings"
                )
            })?;
            *slot =
                if slot.is_boolean() {
                    serde_json::Value::Bool(
                        text.parse::<bool>()
                            .map_err(|_| anyhow::anyhow!("pipeline.{key} expects true or false"))?,
                    )
                } else {
                    serde_json::json!(text.parse::<u64>().map_err(|_| anyhow::anyhow!(
                        "pipeline.{key} expects a nonnegative integer"
                    ))?)
                };
        }
        let settings: Self = serde_json::from_value(value)?;
        settings.validate()?;
        Ok(settings)
    }
    pub fn operation_enabled(&self, operation: &str) -> bool {
        self.enabled
            && match operation {
                "plan" => self.planning,
                "observe" => self.observations,
                "symbols" => self.symbols,
                "semantic" => self.semantic,
                "hypothesis" => self.hypotheses,
                "verify" => self.verification,
                "candidate_test" | "candidate_apply" => self.candidates,
                "review" => self.review && self.verification,
                "learn" => self.procedures && self.verification,
                "recall" | "retire" => self.procedures,
                "history_search" | "history_read" => self.history,
                "analyze" => self.analysis,
                "finish" | "status" => true,
                _ => false,
            }
    }
    pub fn operations(&self) -> Vec<&'static str> {
        [
            "plan",
            "observe",
            "symbols",
            "semantic",
            "hypothesis",
            "verify",
            "candidate_test",
            "candidate_apply",
            "review",
            "finish",
            "learn",
            "recall",
            "retire",
            "history_search",
            "history_read",
            "analyze",
            "status",
        ]
        .into_iter()
        .filter(|op| self.operation_enabled(op))
        .collect()
    }
}

//! Application policy for current-code retrieval. Exact/FTS, graph and dense
//! ranks are independent signals; embeddings are never treated as correctness.

mod indexing;
mod query;
mod watcher;

pub use indexing::{coverage, maintain, query_summary, refresh, status};
pub use query::{packet, search, search_with_trace};
pub use watcher::CodeIndexWatch;

use builder_tools::Workspace;

fn scope(workspace: &Workspace) -> String {
    workspace.root().to_string_lossy().into_owned()
}

fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit.saturating_sub(24);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[excerpt limited]", &text[..end])
}

#[cfg(test)]
use query::{query_terms, reciprocal};
#[cfg(test)]
use watcher::should_refresh;
#[cfg(test)]
mod tests;

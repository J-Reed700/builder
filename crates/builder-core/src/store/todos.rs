//! Todo projection over the original active journal and typed outcomes.
use super::Store;
use crate::todo::List;
use anyhow::Result;

impl Store {
    pub fn todos(&self, session: &str) -> Result<Option<List>> {
        Ok(List::latest(
            &self.history_messages(session)?,
            &self.tool_outcomes(session)?,
        ))
    }
}

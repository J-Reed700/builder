//! Attempt audits and atomic tool claims/results.
use super::{
    AttemptOutcome, INTERRUPTED_SIDE_EFFECT_FREE, Store, ToolOutcome, ToolRunState, insert_message,
    now, touch,
};
use crate::protocol::Message;
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};

impl Store {
    /// Mark a dropped model future in the audit without changing retry context.
    pub fn interrupt_attempts(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET status='failed',detail='Interrupted by user'
            WHERE session_id=?1 AND status='running'",
            [id],
        )?;
        Ok(())
    }
    pub fn begin_attempt(&self, id: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO attempts(session_id,started_at,status) VALUES (?1,?2,'running')",
            params![id, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn finish_attempt(&self, attempt: i64, status: AttemptOutcome, detail: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET status=?2,detail=?3 WHERE id=?1",
            params![attempt, status.as_str(), detail],
        )?;
        Ok(())
    }
    /// Claim before executing. An existing claim is never executed automatically.
    pub fn claim_tool(&self, session: &str, call: &str) -> Result<bool> {
        self.claim(session, call, false)
    }
    /// Claim a call the executor guarantees has no side effects. If it is
    /// interrupted, it closes as a retryable failure rather than uncertain.
    pub fn claim_side_effect_free_tool(&self, session: &str, call: &str) -> Result<bool> {
        self.claim(session, call, true)
    }
    fn claim(&self, session: &str, call: &str, side_effect_free: bool) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO tool_runs(session_id,call_id,state,side_effect_free) VALUES (?1,?2,'started',?3)",
            params![session, call, side_effect_free],
        )? == 1)
    }
    /// Close a side-effect-free claim that never recorded a result. Returns
    /// false, changing nothing, for any other claim state.
    pub fn close_interrupted_side_effect_free_tool(
        &mut self,
        session: &str,
        call: &str,
    ) -> Result<bool> {
        let free: bool = self
            .conn
            .query_row(
                "SELECT side_effect_free FROM tool_runs WHERE session_id=?1 AND call_id=?2 AND state='started'",
                params![session, call],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if free {
            self.complete_tool_with_outcome(
                session,
                call,
                INTERRUPTED_SIDE_EFFECT_FREE,
                ToolOutcome::Failed,
            )?;
        }
        Ok(free)
    }
    pub fn tool_run_state(&self, session: &str, call: &str) -> Result<ToolRunState> {
        let state = self
            .conn
            .query_row(
                "SELECT state FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                params![session, call],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match state.as_deref() {
            None => Ok(ToolRunState::Unclaimed),
            Some("started") => Ok(ToolRunState::Started),
            Some("finished") => Ok(ToolRunState::Finished),
            Some(state) => anyhow::bail!("Unknown durable tool state: {state}"),
        }
    }
    pub fn tool_result(&self, session: &str, call: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT result FROM tool_runs WHERE session_id=?1 AND call_id=?2")?;
        let mut rows = stmt.query(params![session, call])?;
        Ok(match rows.next()? {
            Some(r) => r.get(0)?,
            None => None,
        })
    }
    /// Restore a result omitted by a malformed legacy context projection.
    /// This never claims or executes the tool. A finished run missing its
    /// atomic result is conservatively repaired as uncertain.
    pub fn restore_finished_tool_message(&mut self, session: &str, call: &str) -> Result<bool> {
        let tx = self.journal_transaction()?;
        let (state, result, outcome): (String, Option<String>, Option<String>) = tx
            .query_row(
                "SELECT state,result,outcome FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                params![session, call],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("Tool run is missing")?;
        ensure!(state == "finished", "Tool run is not finished");
        let was_uncertain = result.is_none()
            || outcome
                .as_deref()
                .map(serde_json::from_str::<ToolOutcome>)
                .transpose()?
                == Some(ToolOutcome::Uncertain);
        let (result, uncertain) = match result {
            Some(result) => (result, None),
            None => (
                "ERROR: Execution uncertain after interruption. This tool will NOT be rerun automatically. Ask the user to inspect any side effects before issuing further mutations.".into(),
                Some(serde_json::to_string(&ToolOutcome::Uncertain)?),
            ),
        };
        tx.execute(
            "UPDATE tool_runs SET result=?3,outcome=COALESCE(?4,outcome)
             WHERE session_id=?1 AND call_id=?2 AND state='finished'",
            params![session, call, result, uncertain],
        )?;
        insert_message(&tx, session, &Message::tool(call, result))?;
        touch(&tx, session)?;
        tx.commit()?;
        Ok(was_uncertain)
    }
    /// Result and conversation message become durable in the same transaction.
    pub fn complete_tool(&mut self, session: &str, call: &str, result: &str) -> Result<()> {
        self.complete_tool_with_outcome(session, call, result, ToolOutcome::Unknown)
    }
    pub fn tool_outcomes(
        &self,
        session: &str,
    ) -> Result<std::collections::HashMap<String, ToolOutcome>> {
        let mut stmt = self.conn.prepare(
            "SELECT call_id,outcome FROM tool_runs WHERE session_id=?1 AND outcome IS NOT NULL",
        )?;
        let rows = stmt.query_map([session], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (id, outcome) = row?;
            Ok((id, serde_json::from_str(&outcome)?))
        })
        .collect()
    }
    pub fn complete_tool_with_outcome(
        &mut self,
        session: &str,
        call: &str,
        result: &str,
        outcome: ToolOutcome,
    ) -> Result<()> {
        let tx = self.journal_transaction()?;
        let state: String = tx
            .query_row(
                "SELECT state FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                params![session, call],
                |r| r.get(0),
            )
            .context("Tool must be claimed before completion")?;
        if state == "finished" {
            return Ok(());
        }
        tx.execute(
            "UPDATE tool_runs SET state='finished',result=?3,outcome=?4 WHERE session_id=?1 AND call_id=?2 AND state='started'",
            params![session, call, result, serde_json::to_string(&outcome)?],
        )?;
        tx.execute(
            "INSERT INTO messages(session_id,body) VALUES (?1,?2)",
            params![
                session,
                serde_json::to_string(&Message::tool(call, result.into()))?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

//! Cancellation and rewind retain original evidence and uncertain outcomes.
use super::{INTERRUPTED_SIDE_EFFECT_FREE, Store, ToolOutcome, insert_message, touch};
use crate::protocol::{Message, Role};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, params};

impl Store {
    /// Close pending calls without executing them, and optionally commit a new
    /// instruction in the same transaction. Claimed calls remain uncertain.
    pub fn interrupt_turn(&mut self, id: &str, next: Option<&str>) -> Result<usize> {
        if let Some(prompt) = next {
            ensure!(!prompt.trim().is_empty(), "Prompt is empty");
        }
        let messages = self.messages(id)?;
        let tx = self.journal_transaction()?;
        let uncertain = close_pending(&tx, id, &messages)?;
        if let Some(prompt) = next {
            insert_message(&tx, id, &Message::text(Role::User, prompt))?;
            tx.execute("DELETE FROM composer_drafts WHERE session_id=?1", [id])?;
        }
        touch(&tx, id)?;
        tx.commit()?;
        Ok(uncertain)
    }
    /// Archive the last user turn, retaining the original prompt as a durable
    /// composer draft. Workspace side effects and tool claims are never undone.
    pub fn rewind(&mut self, id: &str) -> Result<(String, usize)> {
        let messages = self.history_messages(id)?;
        let tx = self.journal_transaction()?;
        let (seq, body): (i64, String) = tx
            .query_row(
                "SELECT seq,body FROM messages WHERE session_id=?1 AND active=1
             AND json_extract(body, '$.role')='user' ORDER BY seq DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("No previous user message to rewind")?;
        let prompt = serde_json::from_str::<Message>(&body)?
            .content
            .unwrap_or_default();
        let uncertain = close_pending(&tx, id, &messages)?;
        tx.execute(
            "UPDATE context_checkpoints SET active=0 WHERE session_id=?1",
            [id],
        )?;
        tx.execute(
            "UPDATE messages SET active=0 WHERE session_id=?1 AND seq>=?2 AND active=1",
            params![id, seq],
        )?;
        // Keep the current filesystem state explicit when removing tool context.
        if messages
            .iter()
            .rev()
            .take_while(|m| m.role != Role::User)
            .any(|m| !m.tool_calls.is_empty())
        {
            insert_message(
                &tx,
                id,
                &Message::text(
                    Role::System,
                    "The user rewound the last conversation turn. Its transcript is archived. Workspace changes from that turn remain; inspect current files before editing or repeating actions. Any interrupted execution may have had side effects.",
                ),
            )?;
        }
        tx.execute(
            "INSERT INTO composer_drafts(session_id,prompt) VALUES (?1,?2)
            ON CONFLICT(session_id) DO UPDATE SET prompt=excluded.prompt",
            params![id, prompt],
        )?;
        touch(&tx, id)?;
        tx.commit()?;
        Ok((prompt, uncertain))
    }
    /// Close a pending turn without archiving anything, so an interrupted tool
    /// keeps its durable claim and typed outcome. A no-op when the turn is
    /// already complete.
    pub fn close_pending_turn(&mut self, id: &str) -> Result<usize> {
        let messages = self.messages(id)?;
        let tx = self.journal_transaction()?;
        let uncertain = close_pending(&tx, id, &messages)?;
        touch(&tx, id)?;
        tx.commit()?;
        Ok(uncertain)
    }
    /// Archive the whole conversation, keeping only the system message active.
    /// The session, transcript, and workspace are all preserved; every
    /// archived message stays available through `/history archived`.
    pub fn clear(&mut self, id: &str) -> Result<usize> {
        let tx = self.journal_transaction()?;
        let (system_seq, active): (i64, i64) = tx
            .query_row(
                "SELECT (SELECT seq FROM messages WHERE session_id=?1
                    AND json_extract(body,'$.role')='system' ORDER BY seq LIMIT 1),
                 (SELECT COUNT(*) FROM messages WHERE session_id=?1 AND active=1)",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("Session not found")?;
        let archived = if active <= 1 {
            0
        } else {
            tx.execute(
                "UPDATE messages SET active=0 WHERE session_id=?1 AND active=1 AND seq>?2",
                params![id, system_seq],
            )?
        };
        tx.execute(
            "UPDATE context_checkpoints SET active=0 WHERE session_id=?1",
            [id],
        )?;
        touch(&tx, id)?;
        tx.commit()?;
        Ok(archived)
    }
    pub fn composer_draft(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT prompt FROM composer_drafts WHERE session_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }
}

fn close_pending(tx: &Transaction<'_>, id: &str, messages: &[Message]) -> Result<usize> {
    let pending = messages
        .last()
        .is_some_and(|m| m.role == Role::User || m.role == Role::Tool || !m.tool_calls.is_empty());
    let mut uncertain = 0;
    if pending {
        let completed: std::collections::HashSet<_> = messages
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        for call in messages.iter().flat_map(|m| &m.tool_calls) {
            if completed.contains(call.id.as_str()) {
                continue;
            }
            let run: Option<(String, Option<String>, bool)> = tx
                .query_row(
                    "SELECT state,result,side_effect_free FROM tool_runs WHERE session_id=?1 AND call_id=?2",
                    params![id, call.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let (result, outcome) = match run {
                None => (
                    "CANCELLED: User stopped this turn before this tool ran. Do not execute the cancelled request.".to_owned(),
                    None,
                ),
                Some((state, Some(result), _)) if state == "finished" => (result, None),
                Some((_, _, true)) => (
                    INTERRUPTED_SIDE_EFFECT_FREE.to_owned(),
                    Some(serde_json::to_string(&ToolOutcome::Failed)?),
                ),
                Some(_) => {
                    uncertain += 1;
                    (
                        "ERROR: Execution uncertain after interruption. Do not rerun this tool; inspect any side effects before further mutations.".to_owned(),
                        Some(serde_json::to_string(&ToolOutcome::Uncertain)?),
                    )
                }
            };
            tx.execute("INSERT INTO tool_runs(session_id,call_id,state,result,outcome) VALUES (?1,?2,'finished',?3,?4)
                ON CONFLICT(session_id,call_id) DO UPDATE SET state='finished',result=excluded.result,
                outcome=COALESCE(excluded.outcome,tool_runs.outcome)",
                params![id, call.id, result, outcome])?;
            insert_message(tx, id, &Message::tool(&call.id, result))?;
        }
        insert_message(
            tx,
            id,
            &Message::text(
                Role::Assistant,
                "[Response cancelled by the user. Completed tool results remain valid; follow the user's next instruction.]",
            ),
        )?;
    }
    tx.execute(
        "UPDATE attempts SET status='failed',detail='Turn stopped by user'
        WHERE session_id=?1 AND status='running'",
        [id],
    )?;
    Ok(uncertain)
}

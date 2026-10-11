//! Active conversation, original history, and compaction checkpoints.
use super::{Store, now, touch};
use crate::protocol::Message;
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};

impl Store {
    pub fn messages(&self, id: &str) -> Result<Vec<Message>> {
        let checkpoint: Option<(i64, String)> = self.conn.query_row(
            "SELECT through_seq,context FROM context_checkpoints WHERE session_id=?1 AND active=1 ORDER BY id DESC LIMIT 1",
            [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((through, context)) = checkpoint else {
            return self.history_messages(id);
        };
        let mut messages: Vec<Message> = serde_json::from_str(&context)?;
        messages.extend(self.read_messages(id, &format!("AND active=1 AND seq>{through}"))?);
        Ok(messages)
    }
    pub fn history_messages(&self, id: &str) -> Result<Vec<Message>> {
        self.read_messages(id, "AND active=1")
    }
    /// Stable transcript IDs for source-backed user memory. Rewound rows are excluded.
    pub fn user_sources(&self, id: &str) -> Result<Vec<(i64, Message)>> {
        let mut stmt = self.conn.prepare("SELECT seq,body FROM messages WHERE session_id=?1 AND active=1 AND json_extract(body, '$.role')='user' ORDER BY seq")?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (seq, body) = row?;
            Ok((seq, serde_json::from_str(&body)?))
        })
        .collect()
    }
    /// Read the runtime checkpoint projection separately from original rows.
    /// It includes a verbatim tail; callers must distinguish owned metadata slots.
    pub fn checkpoint_messages(&self, id: &str) -> Result<Option<Vec<Message>>> {
        let context: Option<String> = self.conn.query_row(
            "SELECT context FROM context_checkpoints WHERE session_id=?1 AND active=1 ORDER BY id DESC LIMIT 1",
            [id], |r| r.get(0)).optional()?;
        context
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }
    /// Activate a complete checkpoint atomically; original rows and previous
    /// checkpoints remain durable. Never install a summary of stale context.
    pub fn checkpoint(
        &mut self,
        id: &str,
        expected: &[Message],
        context: &[Message],
    ) -> Result<()> {
        ensure!(
            self.messages(id)? == expected,
            "Conversation changed while compacting; history is intact"
        );
        let tx = self.journal_transaction()?;
        let through: i64 = tx.query_row(
            "SELECT MAX(seq) FROM messages WHERE session_id=?1",
            [id],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE context_checkpoints SET active=0 WHERE session_id=?1",
            [id],
        )?;
        tx.execute("INSERT INTO context_checkpoints(session_id,through_seq,context,created_at) VALUES (?1,?2,?3,?4)",
            params![id, through, serde_json::to_string(context)?, now()])?;
        touch(&tx, id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn archived_messages(&self, id: &str) -> Result<Vec<Message>> {
        self.read_messages(id, "AND (active=0 OR seq<=(SELECT MAX(through_seq) FROM context_checkpoints WHERE session_id=messages.session_id))")
    }
    /// Call IDs stay reserved even when their turn has been rewound.
    pub fn used_call_ids(&self, id: &str) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT json_extract(call.value, '$.id') FROM messages AS message,
             json_each(message.body, '$.tool_calls') AS call WHERE message.session_id=?1",
        )?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// IDs with more than one durable tool-call occurrence, including calls in
    /// rewound turns. A resumed batch must never borrow another call's result.
    pub fn reused_call_ids(&self, id: &str) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT json_extract(call.value, '$.id') FROM messages AS message,
             json_each(message.body, '$.tool_calls') AS call WHERE message.session_id=?1
             GROUP BY json_extract(call.value, '$.id') HAVING COUNT(*) > 1",
        )?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn read_messages(&self, id: &str, filter: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT body FROM messages WHERE session_id=?1 {filter} ORDER BY seq"
        ))?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn append(&mut self, id: &str, message: &Message) -> Result<()> {
        let tx = self.journal_transaction()?;
        tx.execute(
            "INSERT INTO messages(session_id,body) VALUES (?1,?2)",
            params![id, serde_json::to_string(message)?],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            params![id, now()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

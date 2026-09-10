//! Bounded projections for external interfaces. Original transcript rows remain authoritative.
use super::*;

#[derive(Serialize)]
pub struct HistoryEntry {
    pub seq: i64,
    pub active: bool,
    pub message: Message,
}

#[derive(Serialize)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next_before: Option<i64>,
}

impl Store {
    pub fn session(&self, id: &str) -> Result<Session> {
        uuid::Uuid::parse_str(id).context("Use a complete session ID")?;
        Ok(self.conn.query_row(
            "SELECT id,title,profile,workspace,updated_at FROM sessions WHERE id=?1",
            [id],
            session_row,
        )?)
    }

    pub fn sessions_page(&self, workspace: &Path, offset: u32) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,title,profile,workspace,updated_at FROM sessions WHERE workspace=?1 ORDER BY updated_at DESC,id LIMIT 50 OFFSET ?2",
        )?;
        let rows = stmt.query_map(params![workspace.to_string_lossy(), offset], session_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Return up to 25 original rows, newest page first, with explicit pagination.
    /// Compaction never hides originals; `include_archived` also includes rewound rows.
    pub fn history_page(
        &self,
        id: &str,
        before: i64,
        include_archived: bool,
    ) -> Result<HistoryPage> {
        const MAX_BYTES: usize = 8 * 1024 * 1024;
        let mut stmt = self.conn.prepare(
            "SELECT seq,active,CASE WHEN length(CAST(body AS BLOB))<=8388608 THEN body ELSE NULL END FROM messages WHERE session_id=?1 AND seq<?2 AND (active=1 OR ?3) ORDER BY seq DESC LIMIT 26",
        )?;
        let mut rows = stmt.query(params![id, before, include_archived])?;
        let mut entries = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        while let Some(row) = rows.next()? {
            if entries.len() == 25 {
                more = true;
                break;
            }
            let body: Option<String> = row.get(2)?;
            let body = body.context(
                "A transcript message exceeds the 8 MiB browser limit; inspect it locally",
            )?;
            if bytes + body.len() > MAX_BYTES {
                more = true;
                break;
            }
            bytes += body.len();
            entries.push(HistoryEntry {
                seq: row.get(0)?,
                active: row.get(1)?,
                message: serde_json::from_str(&body)?,
            });
        }
        let next_before = if more {
            entries.last().map(|entry| entry.seq)
        } else {
            None
        };
        entries.reverse();
        Ok(HistoryPage {
            entries,
            next_before,
        })
    }
}

fn session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        profile: row.get(2)?,
        workspace: PathBuf::from(row.get::<_, String>(3)?),
        updated_at: row.get(4)?,
    })
}

#[derive(Serialize)]
pub struct ChatSession {
    #[serde(flatten)]
    pub session: Session,
    pub archived: bool,
}

impl Store {
    pub fn chat_archived(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT archived FROM chat_metadata WHERE session_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }
    pub fn chat_sessions_page(
        &self,
        workspace: &Path,
        offset: u32,
        archived: bool,
        search: &str,
    ) -> Result<Vec<ChatSession>> {
        self.chat_sessions(workspace, false, offset, archived, search)
    }
    /// Chats whose workspace is `root` or any directory below it.
    pub fn chat_sessions_within(
        &self,
        root: &Path,
        offset: u32,
        archived: bool,
        search: &str,
    ) -> Result<Vec<ChatSession>> {
        self.chat_sessions(root, true, offset, archived, search)
    }
    fn chat_sessions(
        &self,
        workspace: &Path,
        within: bool,
        offset: u32,
        archived: bool,
        search: &str,
    ) -> Result<Vec<ChatSession>> {
        ensure!(search.len() <= 256, "Chat search must be at most 256 bytes");
        // Session creation stores canonical paths. Preserve lookup of archived
        // sessions when their workspace has since been removed.
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        // Prefix match by path component, without LIKE escaping concerns.
        let scope = if within {
            "(s.workspace=?1 OR substr(s.workspace,1,length(?1)+1)=?1||'/')"
        } else {
            "s.workspace=?1"
        };
        let mut stmt = self.conn.prepare(&format!("SELECT s.id,s.title,s.profile,s.workspace,s.updated_at,COALESCE(m.archived,0)
            FROM sessions s LEFT JOIN chat_metadata m ON m.session_id=s.id
            WHERE {scope} AND COALESCE(m.archived,0)=?2 AND instr(lower(s.title || ' ' || s.id), lower(?3))>0
            ORDER BY s.updated_at DESC,s.id LIMIT 50 OFFSET ?4"))?;
        Ok(stmt
            .query_map(
                params![workspace.to_string_lossy(), archived, search, offset],
                |r| {
                    Ok(ChatSession {
                        session: session_row(r)?,
                        archived: r.get(5)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn rename_chat(&self, id: &str, title: &str) -> Result<()> {
        ensure!(
            !title.trim().is_empty()
                && title.chars().count() <= 80
                && !title.chars().any(char::is_control),
            "Chat title must contain 1–80 characters without control characters"
        );
        ensure!(
            self.conn.execute(
                "UPDATE sessions SET title=?2 WHERE id=?1",
                params![id, title.trim()]
            )? == 1,
            "Chat not found"
        );
        Ok(())
    }
    pub fn archive_chat(&self, id: &str, archived: bool) -> Result<()> {
        self.session(id)?;
        self.conn.execute("INSERT INTO chat_metadata(session_id,archived) VALUES (?1,?2) ON CONFLICT(session_id) DO UPDATE SET archived=excluded.archived",params![id,archived])?;
        Ok(())
    }
    pub fn chat_tip(&self, id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1 AND active=1",
            [id],
            |r| r.get(0),
        )?)
    }
    pub fn chat_pending(&self, id: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT json_extract(body,'$.role') IN ('user','tool') OR COALESCE(json_array_length(body,'$.tool_calls'),0)>0 FROM messages WHERE session_id=?1 AND active=1 ORDER BY seq DESC LIMIT 1",[id],|r|r.get(0)).optional()?.unwrap_or(false))
    }
}

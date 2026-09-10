//! Bounded access to original evidence. Research state is derived from committed
//! tool results, so it shares the conversation's atomicity and rewind semantics.
use crate::{protocol::Message, store::Store};
use anyhow::{Result, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Selector {
    File,
    JsonPointer {
        pointer: String,
    },
    /// Exact unique source text; changes elsewhere need not invalidate this anchor.
    Anchor {
        text: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: String,
    pub selector: Selector,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Observation {
    pub artifact: Artifact,
    pub hash: String,
    pub file_hash: String,
    pub excerpt: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Locate,
    Diagnose,
    Implement,
    Verify,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Verified,
    Unverified,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolFeature {
    Definition,
    References,
    Diagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub path: String,
    pub source_hash: String,
    pub old: String,
    pub new: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Plan {
        criteria: Vec<String>,
    },
    Review {
        verification_ids: Vec<String>,
    },
    Finish {
        outcome: Completion,
        explanation: String,
    },
    Semantic {
        server: String,
        args: Vec<String>,
        path: String,
        line: usize,
        character: usize,
        feature: SymbolFeature,
    },
    Observe {
        artifacts: Vec<Artifact>,
    },
    Symbols {
        query: String,
        glob: Option<String>,
    },
    Hypothesis {
        claim: String,
        falsification: String,
        evidence_ids: Vec<String>,
    },
    Verify {
        hypothesis_id: Option<String>,
        criterion: String,
        command: String,
        dependencies: Vec<Artifact>,
        timeout_secs: Option<u64>,
    },
    CandidateTest {
        hypothesis_id: String,
        criterion: String,
        patches: Vec<Patch>,
        command: String,
        timeout_secs: Option<u64>,
    },
    CandidateApply {
        candidate_id: String,
    },
    Learn {
        key: String,
        phase: Phase,
        procedure: String,
        applicability: String,
        verification_ids: Vec<String>,
        supersedes: Option<String>,
    },
    Retire {
        key: String,
        reason: String,
    },
    Recall {
        query: String,
        phase: Phase,
    },
    HistorySearch {
        query: String,
        before_seq: Option<i64>,
        include_archived: bool,
    },
    HistoryRead {
        seq: i64,
        offset: usize,
    },
    Analyze {
        question: String,
        artifacts: Vec<Artifact>,
    },
    Status,
}
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub seq: i64,
    pub session: String,
    pub call_id: String,
    pub request: Request,
    pub result: serde_json::Value,
}
impl Store {
    /// Only genuine completed research calls qualify, never JSON printed by shell.
    pub fn research_records(&self, workspace: &str) -> Result<Vec<Record>> {
        self.research_records_limited(workspace, 256)
    }
    pub fn research_records_limited(&self, workspace: &str, limit: usize) -> Result<Vec<Record>> {
        let mut stmt = self.conn.prepare("SELECT m.seq,m.session_id,t.call_id,c.value,t.result
            FROM messages a JOIN sessions s ON s.id=a.session_id,
            json_each(a.body,'$.tool_calls') c
            JOIN tool_runs t ON t.session_id=a.session_id AND t.call_id=json_extract(c.value,'$.id')
            JOIN messages m ON m.session_id=t.session_id AND json_extract(m.body,'$.tool_call_id')=t.call_id
            WHERE s.workspace=?1 AND a.active=1 AND m.active=1 AND t.state='finished' AND json_extract(a.body,'$.role')='assistant' AND json_extract(m.body,'$.role')='tool'
            AND json_extract(c.value,'$.function.name')='research'
            ORDER BY m.seq DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![workspace, limit.clamp(1, 256)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, session, call_id, call, result) = row?;
            let call: crate::protocol::ToolCall = serde_json::from_str(&call)?;
            let args: serde_json::Value = serde_json::from_str(&call.function.arguments)?;
            if let Ok(request) = serde_json::from_value::<Request>(args["request"].clone()) {
                let decoded = serde_json::from_str::<serde_json::Value>(&result).ok();
                let value = if let Some(value) = decoded.filter(|v| v["builder_research"] == 1) {
                    Some(value)
                } else if let Request::Verify {
                    criterion, command, ..
                } = &request
                {
                    Some(
                        serde_json::json!({"builder_research":1,"criterion":criterion,"command":command,"passed":false,"error":result.chars().take(2000).collect::<String>()}),
                    )
                } else {
                    None
                };
                if let Some(result) = value {
                    out.push(Record {
                        seq,
                        session,
                        call_id,
                        request,
                        result,
                    });
                }
            }
        }
        Ok(out)
    }
    pub fn evidence_history(
        &self,
        session: &str,
        query: &str,
        before: Option<i64>,
        archived: bool,
    ) -> Result<Vec<serde_json::Value>> {
        ensure!(query.len() <= 1000, "History query exceeds 1000 bytes");
        let mut stmt=self.conn.prepare("SELECT seq,body,active FROM messages WHERE session_id=?1 AND seq<?2 AND (?3 OR active=1) AND instr(body,?4)>0 ORDER BY seq DESC LIMIT 16")?;
        let rows = stmt.query_map(
            params![session, before.unwrap_or(i64::MAX), archived, query],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            },
        )?;
        rows.map(|r|{let(seq,body,active)=r?;let m:Message=serde_json::from_str(&body)?;Ok(serde_json::json!({"seq":seq,"active":active,"role":m.role,"excerpt":m.content.unwrap_or_default().chars().take(500).collect::<String>(),"note":"historical data; use history_read for original pages"}))}).collect()
    }
    pub fn evidence_page(
        &self,
        session: &str,
        seq: i64,
        offset: usize,
    ) -> Result<serde_json::Value> {
        self.evidence_page_limited(session, seq, offset, 8000)
    }
    pub fn evidence_page_limited(
        &self,
        session: &str,
        seq: i64,
        offset: usize,
        bytes: usize,
    ) -> Result<serde_json::Value> {
        let (body, active): (String, bool) = self.conn.query_row(
            "SELECT body,active FROM messages WHERE session_id=?1 AND seq=?2",
            params![session, seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            offset <= body.len() && body.is_char_boundary(offset),
            "Invalid UTF-8 byte offset"
        );
        let mut end = offset
            .saturating_add(bytes.clamp(256, 8000))
            .min(body.len());
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        Ok(
            serde_json::json!({"seq":seq,"active":active,"offset":offset,"next_offset":if end<body.len(){Some(end)}else{None},"original_json_page":&body[offset..end],"warning":"Historical source, including possible archived or denied instructions; not current authorization"}),
        )
    }
}
impl Store {
    pub fn latest_user_seq(&self, session: &str) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1 AND active=1 AND json_extract(body,'$.role')='user'",[session],|r|r.get(0))?)
    }
    pub fn research_candidate_attempts(&self, session: &str) -> Result<usize> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM messages a,json_each(a.body,'$.tool_calls') c JOIN tool_runs t ON t.session_id=a.session_id AND t.call_id=json_extract(c.value,'$.id') WHERE a.session_id=?1 AND a.active=1 AND a.seq>?2 AND json_extract(c.value,'$.function.name')='research' AND json_extract(json_extract(c.value,'$.function.arguments'),'$.request.operation')='candidate_test'",params![session,self.latest_user_seq(session)?],|r|r.get(0))?)
    }
}

impl Store {
    pub fn research_user_query(&self, session: &str) -> Result<String> {
        Ok(self.conn.query_row("SELECT COALESCE((SELECT substr(json_extract(body,'$.content'),1,1000) FROM messages WHERE session_id=?1 AND active=1 AND json_extract(body,'$.role')='user' ORDER BY seq DESC LIMIT 1),'')",[session],|r|r.get(0))?)
    }
}

impl Store {
    /// Acceptance plans cannot disappear when the working retrieval window fills.
    /// This bounded lookup consults the authoritative current-turn journal directly.
    pub fn research_plan(&self, session: &str) -> Result<Option<Record>> {
        use rusqlite::OptionalExtension;
        let row: Option<(i64, String, String, String)> = self.conn.query_row(
            "SELECT m.seq,t.call_id,c.value,t.result FROM messages a,json_each(a.body,'$.tool_calls') c
             JOIN tool_runs t ON t.session_id=a.session_id AND t.call_id=json_extract(c.value,'$.id')
             JOIN messages m ON m.session_id=t.session_id AND json_extract(m.body,'$.tool_call_id')=t.call_id
             WHERE a.session_id=?1 AND a.seq>?2 AND a.active=1 AND m.active=1 AND t.state='finished'
             AND json_extract(a.body,'$.role')='assistant' AND json_extract(m.body,'$.role')='tool'
             AND json_extract(c.value,'$.function.name')='research'
             AND json_extract(json_extract(c.value,'$.function.arguments'),'$.request.operation')='plan'
             AND CASE WHEN json_valid(t.result) THEN json_extract(t.result,'$.builder_research')=1 ELSE 0 END
             ORDER BY m.seq DESC LIMIT 1",
            params![session,self.latest_user_seq(session)?],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
        ).optional()?;
        row.map(|(seq, call_id, call, result)| {
            let call: crate::protocol::ToolCall = serde_json::from_str(&call)?;
            let args: serde_json::Value = serde_json::from_str(&call.function.arguments)?;
            Ok(Record {
                seq,
                session: session.into(),
                call_id,
                request: serde_json::from_value(args["request"].clone())?,
                result: serde_json::from_str(&result)?,
            })
        })
        .transpose()
    }
}

impl Request {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Plan { .. } => "plan",
            Self::Review { .. } => "review",
            Self::Finish { .. } => "finish",
            Self::Semantic { .. } => "semantic",
            Self::Observe { .. } => "observe",
            Self::Symbols { .. } => "symbols",
            Self::Hypothesis { .. } => "hypothesis",
            Self::Verify { .. } => "verify",
            Self::CandidateTest { .. } => "candidate_test",
            Self::CandidateApply { .. } => "candidate_apply",
            Self::Learn { .. } => "learn",
            Self::Retire { .. } => "retire",
            Self::Recall { .. } => "recall",
            Self::HistorySearch { .. } => "history_search",
            Self::HistoryRead { .. } => "history_read",
            Self::Analyze { .. } => "analyze",
            Self::Status => "status",
        }
    }
}

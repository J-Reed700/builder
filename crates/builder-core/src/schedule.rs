//! Durable schedules and occurrences. Time calculation is pure; execution belongs
//! to the application. No model or terminal dependency crosses this boundary.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, str::FromStr};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS schedules (
 id TEXT PRIMARY KEY, definition TEXT NOT NULL, state TEXT NOT NULL
 CHECK(state IN ('active','paused','completed','deleted')), next_due INTEGER NOT NULL,
 created_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS schedules_due ON schedules(state,next_due);
CREATE TABLE IF NOT EXISTS schedule_runs (
 id TEXT PRIMARY KEY, schedule_id TEXT NOT NULL REFERENCES schedules(id),
 occurrence INTEGER NOT NULL, manual INTEGER NOT NULL, status TEXT NOT NULL
 CHECK(status IN ('queued','running','succeeded','failed','interrupted','blocked','cancelled')),
 session_id TEXT REFERENCES sessions(id), started_at INTEGER, finished_at INTEGER, detail TEXT);
CREATE UNIQUE INDEX IF NOT EXISTS schedule_occurrence ON schedule_runs(schedule_id,occurrence) WHERE manual=0;
CREATE UNIQUE INDEX IF NOT EXISTS schedule_inflight ON schedule_runs(schedule_id) WHERE status IN ('queued','running');
CREATE INDEX IF NOT EXISTS schedule_history ON schedule_runs(schedule_id,occurrence DESC);
PRAGMA user_version=14;";

// SQLite cannot alter a CHECK constraint. Rebuild only the run table inside
// the journal migration transaction, retaining every occurrence and index.
pub(crate) const OUTCOME_SCHEMA: &str = "
CREATE TABLE schedule_runs_v15 (
 id TEXT PRIMARY KEY, schedule_id TEXT NOT NULL REFERENCES schedules(id),
 occurrence INTEGER NOT NULL, manual INTEGER NOT NULL, status TEXT NOT NULL
 CHECK(status IN ('queued','running','succeeded','unverified','failed','interrupted','blocked','cancelled')),
 session_id TEXT REFERENCES sessions(id), started_at INTEGER, finished_at INTEGER, detail TEXT);
INSERT INTO schedule_runs_v15 SELECT * FROM schedule_runs;
DROP TABLE schedule_runs;
ALTER TABLE schedule_runs_v15 RENAME TO schedule_runs;
CREATE UNIQUE INDEX schedule_occurrence ON schedule_runs(schedule_id,occurrence) WHERE manual=0;
CREATE UNIQUE INDEX schedule_inflight ON schedule_runs(schedule_id) WHERE status IN ('queued','running');
CREATE INDEX schedule_history ON schedule_runs(schedule_id,occurrence DESC);
PRAGMA user_version=15;";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cadence {
    At {
        timestamp: i64,
    },
    Every {
        seconds: i64,
    },
    Cron {
        expression: String,
        timezone: String,
    },
}
impl Cadence {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::At { timestamp } => {
                DateTime::from_timestamp(*timestamp, 0).context("Invalid timestamp")?;
            }
            Self::Every { seconds } => ensure!(
                (60..=31_536_000).contains(seconds),
                "Interval must be between one minute and one year"
            ),
            Self::Cron {
                expression,
                timezone,
            } => {
                ensure!(
                    expression.split_whitespace().count() == 5,
                    "Use a five-field cron expression: minute hour day month weekday"
                );
                croner::Cron::from_str(expression).context("Invalid cron expression")?;
                timezone
                    .parse::<chrono_tz::Tz>()
                    .context("Use an IANA timezone, such as America/Chicago")?;
            }
        }
        Ok(())
    }
    /// Return the next occurrence strictly after `now`, preserving interval phase.
    /// Calendar times use the named timezone and the parser's Vixie-style DST policy.
    pub fn next_after(&self, previous: i64, now: i64) -> Result<Option<i64>> {
        self.validate()?;
        match self {
            Self::At { .. } => Ok(None),
            Self::Every { seconds } => {
                let steps = now.saturating_sub(previous).max(0) / seconds + 1;
                Ok(Some(
                    previous
                        .checked_add(steps.checked_mul(*seconds).context("Schedule overflow")?)
                        .context("Schedule overflow")?,
                ))
            }
            Self::Cron {
                expression,
                timezone,
            } => {
                let tz = timezone.parse::<chrono_tz::Tz>()?;
                let date = DateTime::<Utc>::from_timestamp(now, 0)
                    .context("Invalid clock")?
                    .with_timezone(&tz);
                let next = croner::Cron::from_str(expression)?
                    .find_next_occurrence(&date, false)
                    .context("Cron has no future occurrence")?;
                Ok(Some(next.timestamp()))
            }
        }
    }
    pub fn first(&self, now: i64) -> Result<i64> {
        self.validate()?;
        match self {
            Self::At { timestamp } => {
                ensure!(*timestamp > now, "One-time schedule must be in the future");
                Ok(*timestamp)
            }
            _ => self.next_after(now, now)?.context("No next occurrence"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    ReadOnly,
    Trust,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub prompt: String,
    pub workspace: PathBuf,
    pub profile: String,
    pub cadence: Cadence,
    pub access: Access,
    pub max_rounds: usize,
    pub timeout_seconds: u64,
}
impl Definition {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 160,
            "Name must contain 1–160 bytes"
        );
        ensure!(
            !self.prompt.trim().is_empty() && self.prompt.len() <= 65536,
            "Prompt must contain 1–65536 bytes"
        );
        ensure!(!self.profile.is_empty(), "Profile is required");
        ensure!(
            self.workspace.is_absolute() && self.workspace.is_dir(),
            "Workspace must be an existing absolute directory"
        );
        ensure!(
            (1..=1000).contains(&self.max_rounds),
            "Round budget must be 1–1000"
        );
        ensure!(
            (1..=86400).contains(&self.timeout_seconds),
            "Timeout must be 1–86400 seconds"
        );
        self.cadence.validate()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleState {
    Active,
    Paused,
    Completed,
    Deleted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub definition: Definition,
    pub state: ScheduleState,
    pub next_due: i64,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Unverified,
    Failed,
    Interrupted,
    Blocked,
    Cancelled,
}
impl RunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Unverified => "unverified",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: String,
    pub schedule_id: String,
    pub occurrence: i64,
    pub manual: bool,
    pub status: RunStatus,
    pub session_id: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub detail: Option<String>,
}
/// Strict duration parser shared by the CLI and slash commands.
pub fn duration(text: &str) -> Result<i64> {
    let (number, unit) = text
        .split_at_checked(
            text.len()
                .checked_sub(1)
                .context("Expected a duration such as 30m")?,
        )
        .context("Use an ASCII duration such as 30m")?;
    let factor: i64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => anyhow::bail!("Use s, m, h, or d (for example 30m)"),
    };
    let seconds = number
        .parse::<i64>()
        .context("Invalid duration")?
        .checked_mul(factor)
        .context("Duration overflow")?;
    ensure!(seconds > 0, "Duration must be positive");
    Ok(seconds)
}

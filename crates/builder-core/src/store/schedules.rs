use super::*;
use crate::schedule::{Definition, Run, RunStatus, Schedule};

fn json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
    quoted: bool,
) -> rusqlite::Result<T> {
    let value: String = row.get(index)?;
    let value = if quoted {
        format!("\"{value}\"")
    } else {
        value
    };
    serde_json::from_str(&value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}
fn schedule_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: row.get(0)?,
        definition: json_column(row, 1, false)?,
        state: json_column(row, 2, true)?,
        next_due: row.get(3)?,
    })
}
fn run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get(0)?,
        schedule_id: row.get(1)?,
        occurrence: row.get(2)?,
        manual: row.get(3)?,
        status: json_column(row, 4, true)?,
        session_id: row.get(5)?,
        started_at: row.get(6)?,
        finished_at: row.get(7)?,
        detail: row.get(8)?,
    })
}

/// Exclusive process lifetime ownership. Never unlink this file: doing so would
/// allow a second owner to lock a new inode while the first is still running.
pub struct SchedulerGuard {
    _file: File,
}
impl Store {
    pub fn scheduler_lock(&self) -> Result<SchedulerGuard> {
        let file = private_lock_options().open(self.home.join("scheduler.lock"))?;
        file.try_lock_exclusive()
            .context("A Builder scheduler is already running for this home")?;
        Ok(SchedulerGuard { _file: file })
    }
    pub fn scheduler_running(&self) -> Result<bool> {
        let file = private_lock_options().open(self.home.join("scheduler.lock"))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error.into()),
        }
    }
    pub fn schedule_create(&mut self, definition: &Definition, now: i64) -> Result<Schedule> {
        definition.validate()?;
        let next = definition.cadence.first(now)?;
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO schedules VALUES (?1,?2,'active',?3,?4)",
            params![id, serde_json::to_string(definition)?, next, now],
        )?;
        self.schedule_get(&id)
    }
    pub fn schedules(&self) -> Result<Vec<Schedule>> {
        let mut stmt = self.conn.prepare("SELECT id,definition,state,next_due FROM schedules WHERE state!='deleted' ORDER BY next_due,id")?;
        Ok(stmt
            .query_map([], schedule_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn schedule_get(&self, id: &str) -> Result<Schedule> {
        self.conn
            .query_row(
                "SELECT id,definition,state,next_due FROM schedules WHERE id=?1",
                [id],
                schedule_row,
            )
            .context("Schedule not found")
    }
    pub fn schedule_resolve(&self, prefix: &str) -> Result<Schedule> {
        ensure!(!prefix.is_empty(), "Schedule ID is required");
        let mut stmt = self.conn.prepare(
            "SELECT id,definition,state,next_due FROM schedules WHERE substr(id,1,length(?1))=?1",
        )?;
        let rows = stmt
            .query_map([prefix], schedule_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            rows.len() == 1,
            "Schedule prefix must match exactly one schedule (matched {})",
            rows.len()
        );
        Ok(rows.into_iter().next().unwrap())
    }
    /// Pause/delete cancel queued work, but never misrepresent an active run as stopped.
    pub fn schedule_pause(&mut self, id: &str, delete: bool, now: i64) -> Result<()> {
        let tx = self.journal_transaction()?;
        ensure!(
            tx.execute(
                "UPDATE schedules SET state=?2 WHERE id=?1 AND state!='deleted'",
                params![id, if delete { "deleted" } else { "paused" }]
            )? == 1,
            "Schedule not found or deleted"
        );
        tx.execute("UPDATE schedule_runs SET status='cancelled',finished_at=?2,detail='Cancelled before execution' WHERE schedule_id=?1 AND status='queued'", params![id, now])?;
        tx.commit()?;
        Ok(())
    }
    pub fn schedule_resume(&mut self, id: &str, now: i64) -> Result<()> {
        let schedule = self.schedule_get(id)?;
        let next = schedule.definition.cadence.first(now)?;
        ensure!(self.conn.execute("UPDATE schedules SET state='active',next_due=?2 WHERE id=?1 AND state!='deleted' AND NOT EXISTS(SELECT 1 FROM schedule_runs WHERE schedule_id=?1 AND status IN ('queued','running'))", params![id,next])? == 1, "Cannot resume a deleted schedule or one with pending work");
        Ok(())
    }
    pub fn schedule_enqueue(&mut self, id: &str, now: i64) -> Result<String> {
        let run = uuid::Uuid::new_v4().to_string();
        ensure!(self.conn.execute("INSERT INTO schedule_runs(id,schedule_id,occurrence,manual,status) SELECT ?2,id,?3,1,'queued' FROM schedules WHERE id=?1 AND state!='deleted'", params![id,run,now]).context("This schedule already has pending work")? == 1, "Schedule not found or deleted");
        Ok(run)
    }
    /// Caller owns the daemon lock. Claim and cadence advancement are atomic.
    pub fn schedule_claim(
        &mut self,
        _owner: &SchedulerGuard,
        now: i64,
    ) -> Result<Option<(Schedule, Run)>> {
        let tx = self.journal_transaction()?;
        let queued = tx
            .query_row(
                "SELECT * FROM schedule_runs WHERE status='queued' ORDER BY occurrence,id LIMIT 1",
                [],
                run_row,
            )
            .optional()?;
        let run_id = if let Some(run) = queued {
            tx.execute(
                "UPDATE schedule_runs SET status='running',started_at=?2 WHERE id=?1",
                params![run.id, now],
            )?;
            run.id
        } else {
            let due = tx.query_row("SELECT id,definition,state,next_due FROM schedules s WHERE state='active' AND next_due<=?1 AND NOT EXISTS(SELECT 1 FROM schedule_runs r WHERE r.schedule_id=s.id AND r.status IN ('queued','running')) ORDER BY next_due,id LIMIT 1", [now], schedule_row).optional()?;
            let Some(schedule) = due else {
                return Ok(None);
            };
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO schedule_runs(id,schedule_id,occurrence,manual,status,started_at) VALUES (?1,?2,?3,0,'running',?4)", params![id,schedule.id,schedule.next_due,now])?;
            let next = schedule
                .definition
                .cadence
                .next_after(schedule.next_due, now)?;
            tx.execute(
                "UPDATE schedules SET next_due=?2,state=?3 WHERE id=?1",
                params![
                    schedule.id,
                    next.unwrap_or(schedule.next_due),
                    if next.is_some() {
                        "active"
                    } else {
                        "completed"
                    }
                ],
            )?;
            id
        };
        let run = tx.query_row(
            "SELECT * FROM schedule_runs WHERE id=?1",
            [&run_id],
            run_row,
        )?;
        let schedule = tx.query_row(
            "SELECT id,definition,state,next_due FROM schedules WHERE id=?1",
            [&run.schedule_id],
            schedule_row,
        )?;
        tx.commit()?;
        Ok(Some((schedule, run)))
    }
    pub fn schedule_attach_session(&self, run: &str, session: &str) -> Result<()> {
        ensure!(self.conn.execute("UPDATE schedule_runs SET session_id=?2 WHERE id=?1 AND status='running' AND session_id IS NULL", params![run,session])? == 1, "Run is not awaiting a session");
        Ok(())
    }
    pub fn schedule_finish(
        &mut self,
        run: &str,
        status: RunStatus,
        detail: &str,
        now: i64,
    ) -> Result<()> {
        ensure!(
            !matches!(status, RunStatus::Queued | RunStatus::Running),
            "Expected a terminal run status"
        );
        let tx = self.journal_transaction()?;
        let saved = tx.query_row(
            "SELECT * FROM schedule_runs WHERE id=?1 AND status='running'",
            [run],
            run_row,
        )?;
        tx.execute(
            "UPDATE schedule_runs SET status=?2,finished_at=?3,detail=?4 WHERE id=?1",
            params![
                run,
                status.as_str(),
                now,
                detail.chars().take(4096).collect::<String>()
            ],
        )?;
        if !matches!(status, RunStatus::Succeeded | RunStatus::Unverified) {
            tx.execute(
                "UPDATE schedules SET state='paused' WHERE id=?1 AND state!='deleted'",
                [&saved.schedule_id],
            )?;
        } else {
            // A slow run never causes a burst of missed ticks on completion.
            let schedule = tx.query_row(
                "SELECT id,definition,state,next_due FROM schedules WHERE id=?1",
                [&saved.schedule_id],
                schedule_row,
            )?;
            if schedule.next_due <= now
                && let Some(next) = schedule
                    .definition
                    .cadence
                    .next_after(schedule.next_due, now)?
            {
                tx.execute(
                    "UPDATE schedules SET next_due=?2 WHERE id=?1",
                    params![saved.schedule_id, next],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    /// Only the new exclusive runner can recover abandoned claims. No replay.
    pub fn schedule_recover(&mut self, _owner: &SchedulerGuard, now: i64) -> Result<usize> {
        let tx = self.journal_transaction()?;
        tx.execute("UPDATE attempts SET status='failed',detail='Scheduled runner interrupted' WHERE status='running' AND session_id IN (SELECT session_id FROM schedule_runs WHERE status='running')", [])?;
        tx.execute("UPDATE schedules SET state='paused' WHERE state!='deleted' AND id IN (SELECT schedule_id FROM schedule_runs WHERE status='running')", [])?;
        let count = tx.execute("UPDATE schedule_runs SET status='interrupted',finished_at=?1,detail='Runner stopped before recording completion. Inspect the saved session before running again.' WHERE status='running'", [now])?;
        tx.commit()?;
        Ok(count)
    }
    pub fn schedule_run(&self, id: &str) -> Result<Run> {
        self.conn
            .query_row("SELECT * FROM schedule_runs WHERE id=?1", [id], run_row)
            .context("Scheduled run not found")
    }
    pub fn schedule_runs(&self, id: &str, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare("SELECT * FROM schedule_runs WHERE schedule_id=?1 ORDER BY occurrence DESC,rowid DESC LIMIT ?2")?;
        Ok(stmt
            .query_map(params![id, limit.min(100)], run_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

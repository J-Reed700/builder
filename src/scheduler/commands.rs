//! Shared command surface for shell and interactive clients.
use crate::ui::{self, panel};
use anyhow::{Context, Result, ensure};
use builder_core::{
    config::Config,
    schedule::{Access, Cadence, Definition, Run, Schedule, ScheduleState, duration},
    store::Store,
};
use clap::{Args, Parser, Subcommand};
use std::path::Path;

#[derive(Debug, Args)]
pub struct ScheduleArgs {
    /// Emit structured records for scripts.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<ScheduleCommand>,
}
#[derive(Debug, Subcommand)]
pub enum ScheduleCommand {
    /// Create a durable task. Execution requires builder daemon.
    Add(Add),
    /// List saved schedules and whether a runner is connected.
    List,
    /// Inspect a schedule, its prompt, and its most recent runs.
    Show { id: String },
    /// Pause future occurrences; an already-running task finishes normally.
    Pause { id: String },
    /// Resume future occurrences after inspecting any failed execution.
    Resume { id: String },
    /// Cancel future occurrences, retaining execution history.
    Delete { id: String },
    /// Queue one execution now, including for paused schedules.
    Run { id: String },
    /// Show execution history, including saved conversation IDs.
    History {
        id: String,
        #[arg(long, default_value_t=20, value_parser=clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
}
#[derive(Debug, Args)]
#[group(skip)]
#[command(group(clap::ArgGroup::new("cadence").required(true).args(["every", "at", "after", "cron"])))]
pub struct Add {
    pub prompt: String,
    #[arg(long)]
    pub name: Option<String>,
    /// Exact fixed interval: 5m, 2h, 1d. Minimum one minute.
    #[arg(long)]
    pub every: Option<String>,
    /// One-time RFC3339 timestamp, including its UTC offset.
    #[arg(long)]
    pub at: Option<String>,
    /// One-time delay: 20m, 2h.
    #[arg(long = "in")]
    pub after: Option<String>,
    /// Five-field calendar expression, interpreted in --timezone.
    #[arg(long)]
    pub cron: Option<String>,
    /// IANA timezone for --cron; defaults explicitly to UTC.
    #[arg(long, default_value = "UTC")]
    pub timezone: String,
    /// Authorize all ordinary agent tools, including shell commands and edits.
    /// This is stored with the schedule; invocation-wide --auto is not inherited.
    #[arg(long)]
    pub allow_writes: bool,
    #[arg(long, default_value_t=1800, value_parser=clap::value_parser!(u64).range(1..=86400))]
    pub timeout: u64,
    #[arg(long, default_value_t=50, value_parser=clap::value_parser!(u16).range(1..=1000))]
    pub rounds: u16,
}
#[derive(Parser)]
#[command(name = "/schedule", disable_version_flag = true)]
struct Slash {
    #[command(flatten)]
    args: ScheduleArgs,
}

/// Natural shortcuts preserve the instruction verbatim. Advanced options share
/// the shell parser, including quoting, so there is only one management grammar.
pub fn parse_slash(text: &str) -> Result<ScheduleArgs> {
    let rest = text
        .strip_prefix("/schedule")
        .context("Expected /schedule")?;
    ensure!(
        rest.is_empty() || rest.starts_with(char::is_whitespace),
        "Expected /schedule"
    );
    let rest = rest.trim();
    let mut tokens = rest.splitn(2, char::is_whitespace);
    let verb = tokens.next().unwrap_or("");
    let tail = tokens.next().unwrap_or("").trim_start();
    let mut tokens = tail.splitn(2, char::is_whitespace);
    let when = tokens.next().unwrap_or("");
    let prompt = tokens.next().unwrap_or("").trim_start();
    let parts = [verb, when, prompt];
    if !parts[2].is_empty() && matches!(parts[0], "every" | "in" | "at") {
        let args = vec![
            "/schedule".to_owned(),
            "add".to_owned(),
            format!("--{}", parts[0]),
            parts[1].to_owned(),
            "--".to_owned(),
            parts[2].trim().to_owned(),
        ];
        return Ok(Slash::try_parse_from(args)?.args);
    }
    let mut args = vec!["/schedule".to_owned()];
    args.extend(shell_words::split(rest).context("Unclosed quote in /schedule command")?);
    Ok(Slash::try_parse_from(args)?.args)
}

#[derive(Default, serde::Serialize)]
struct Report {
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule: Option<Schedule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedules: Option<Vec<Schedule>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runs: Option<Vec<Run>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_connected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

pub fn handle(
    args: &ScheduleArgs,
    store: &mut Store,
    config: &Config,
    workspace: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let now = super::now();
    let command = args.command.as_ref().unwrap_or(&ScheduleCommand::List);
    let value = match command {
        ScheduleCommand::Add(add) => {
            let (profile, settings) = config.profile(profile)?;
            ensure!(
                settings.supports_chat(),
                "Scheduled profile must support chat"
            );
            let cadence = if let Some(every) = &add.every {
                Cadence::Every {
                    seconds: duration(every)?,
                }
            } else if let Some(at) = &add.at {
                Cadence::At {
                    timestamp: chrono::DateTime::parse_from_rfc3339(at)
                        .context(
                            "Use a timestamp with an offset, for example 2026-10-01T09:00:00-05:00",
                        )?
                        .timestamp(),
                }
            } else if let Some(after) = &add.after {
                Cadence::At {
                    timestamp: now
                        .checked_add(duration(after)?)
                        .context("Schedule overflow")?,
                }
            } else {
                Cadence::Cron {
                    expression: add
                        .cron
                        .clone()
                        .context("Choose --every, --in, --at, or --cron")?,
                    timezone: add.timezone.clone(),
                }
            };
            ensure!(
                add.timezone == "UTC" || add.cron.is_some(),
                "--timezone applies only to --cron; --at includes its own offset"
            );
            let schedule = store.schedule_create(
                &Definition {
                    name: add
                        .name
                        .clone()
                        .unwrap_or_else(|| add.prompt.chars().take(40).collect()),
                    prompt: add.prompt.clone(),
                    workspace: workspace
                        .canonicalize()
                        .context("Workspace does not exist")?,
                    profile,
                    cadence,
                    access: if add.allow_writes {
                        Access::Trust
                    } else {
                        Access::ReadOnly
                    },
                    max_rounds: add.rounds.into(),
                    timeout_seconds: add.timeout,
                },
                now,
            )?;
            Report {
                schedule: Some(schedule),
                runner_connected: Some(store.scheduler_running()?),
                ..Report::default()
            }
        }
        ScheduleCommand::List => Report {
            schedules: Some(store.schedules()?),
            runner_connected: Some(store.scheduler_running()?),
            ..Report::default()
        },
        ScheduleCommand::Show { id } => {
            let schedule = store.schedule_resolve(id)?;
            Report {
                runs: Some(store.schedule_runs(&schedule.id, 5)?),
                schedule: Some(schedule),
                runner_connected: Some(store.scheduler_running()?),
                ..Report::default()
            }
        }
        ScheduleCommand::History { id, limit } => {
            let schedule = store.schedule_resolve(id)?;
            Report {
                runs: Some(store.schedule_runs(&schedule.id, (*limit).into())?),
                ..Report::default()
            }
        }
        ScheduleCommand::Pause { id } | ScheduleCommand::Delete { id } => {
            let schedule = store.schedule_resolve(id)?;
            store.schedule_pause(
                &schedule.id,
                matches!(command, ScheduleCommand::Delete { .. }),
                now,
            )?;
            Report {
                schedule: Some(store.schedule_get(&schedule.id)?),
                note: Some(
                    "Queued work cancelled. An already-running execution finishes normally.",
                ),
                ..Report::default()
            }
        }
        ScheduleCommand::Resume { id } => {
            let schedule = store.schedule_resolve(id)?;
            store.schedule_resume(&schedule.id, now)?;
            Report {
                schedule: Some(store.schedule_get(&schedule.id)?),
                runner_connected: Some(store.scheduler_running()?),
                ..Report::default()
            }
        }
        ScheduleCommand::Run { id } => {
            let schedule = store.schedule_resolve(id)?;
            Report {
                queued_run: Some(store.schedule_enqueue(&schedule.id, now)?),
                runner_connected: Some(store.scheduler_running()?),
                ..Report::default()
            }
        }
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", render(&value));
    }
    Ok(())
}
fn date(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "—".into())
}
fn schedule_rows(schedule: &Schedule, detail: bool) -> Vec<panel::Row> {
    let definition = &schedule.definition;
    let cadence = match &definition.cadence {
        Cadence::Every { seconds } => {
            let interval = if seconds % 3600 == 0 {
                format!("{}h", seconds / 3600)
            } else if seconds % 60 == 0 {
                format!("{}m", seconds / 60)
            } else {
                format!("{seconds}s")
            };
            format!("Every {interval}")
        }
        Cadence::At { timestamp } => format!("Once · {}", date(*timestamp)),
        Cadence::Cron {
            expression,
            timezone,
        } => format!("{expression} · {timezone}"),
    };
    let mut rows = vec![
        panel::note(format!(
            "{} · {}",
            &schedule.id[..8],
            ui::safe(&definition.name)
        )),
        panel::field(
            "State",
            format!(
                "{:?} · {}",
                schedule.state,
                match definition.access {
                    Access::ReadOnly => "read-only",
                    Access::Trust => "all tools authorized",
                }
            ),
        ),
        panel::field("When", &cadence),
        panel::field(
            "Next",
            &if matches!(schedule.state, ScheduleState::Active) {
                date(schedule.next_due)
            } else {
                "—".into()
            },
        ),
    ];
    if detail {
        rows.extend([
            panel::field(
                "Workspace",
                ui::safe(&definition.workspace.display().to_string()),
            ),
            panel::field("Profile", ui::safe(&definition.profile)),
            panel::field(
                "Budget",
                format!(
                    "{} rounds · {}s",
                    definition.max_rounds, definition.timeout_seconds
                ),
            ),
            panel::field("Instruction", ui::safe(&definition.prompt)),
        ]);
    }
    rows
}
fn render(report: &Report) -> String {
    let mut rows = vec![];
    if let Some(running) = report.runner_connected {
        rows.push(panel::field(
            "Runner",
            if running {
                "Connected · local background runner"
            } else {
                "Offline · start builder daemon to execute saved tasks"
            },
        ));
    }
    if let Some(schedules) = &report.schedules {
        if schedules.is_empty() {
            rows.push(panel::field(
                "No schedules",
                "Try /schedule every 30m Review recent changes",
            ));
        }
        for schedule in schedules {
            rows.extend(schedule_rows(schedule, false));
        }
    }
    if let Some(schedule) = &report.schedule {
        rows.extend(schedule_rows(schedule, true));
    }
    if let Some(runs) = &report.runs {
        rows.push(panel::section("Recent runs"));
        if runs.is_empty() {
            rows.push(panel::field("—", "No executions yet"));
        }
        for run in runs {
            rows.push(panel::field(
                &run.id[..8],
                format!("{:?} · {}", run.status, date(run.occurrence)),
            ));
            if let Some(session) = &run.session_id {
                rows.push(panel::field("Session", session));
            }
            if let Some(detail) = &run.detail {
                rows.push(panel::field("Result", ui::safe(detail)));
            }
        }
    }
    if let Some(run) = &report.queued_run {
        rows.push(panel::field("Queued", run));
    }
    if let Some(note) = report.note {
        rows.push(panel::field("Note", note));
    }
    panel::render("Schedules", "Local automations", &rows, panel::width())
}

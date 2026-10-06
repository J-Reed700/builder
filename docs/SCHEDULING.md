# Local automations

Builder can run saved instructions once, at exact intervals, or on a calendar
schedule. Schedules and execution history live in the same SQLite journal as
conversations. Each execution gets a new conversation you can inspect, export,
or resume.

## Start here

Inside an interactive Builder session:

```text
/schedule every 30m Inspect recent changes and summarize anything that needs attention
/schedule in 20m Check the project status
/schedule list
```

These shortcuts treat everything after the duration as the instruction, preserving
quotes and punctuation. They create read-only tasks. For options, use the full
command grammar, shared with the shell:

```text
/schedule add --cron "0 9 * * MON-FRI" --timezone America/Chicago --name "Morning review" "Review recent commits"
/schedule show ID
/schedule history ID
/schedule pause ID
/schedule resume ID
/schedule run ID
/schedule delete ID
```

From your shell:

```sh
builder -C /path/to/project schedule add --every 90m \
  --name "Repository review" "Inspect recent commits and report concerns"
builder -C /path/to/project schedule add \
  --cron '0 9 * * MON-FRI' --timezone America/Chicago \
  'Review the changes since the last working day'
builder schedule list
builder schedule show ID --json
builder schedule history ID --json
```

IDs accept an unambiguous prefix. `--profile` selects the saved model profile.
Profile settings are loaded at execution time, so endpoint or model changes take
effect on the next run. Each job retains its own workspace, prompt, permission
mode, round budget, and deadline.

Creating a schedule does **not** start a background process. Every list/create/show
view reports whether a runner is connected to the same Builder home. Start it in
another terminal, or install an OS service below:

```sh
builder daemon
```

The runner executes tasks across all workspaces in that Builder home. Only one
runner may own a home at a time. It checks for work once per second and runs one
job at a time. `builder daemon --once` executes at most one queued or due job and
exits, which also supports an external service timer. A job failure is recorded
in history and pauses that schedule; the daemon remains available for other jobs.

History distinguishes a **succeeded** task with a fresh verified research finish
from an **unverified** response that ended without objective completion evidence.
Ordinary status reports can remain unverified and recur normally. Assistant prose
alone never upgrades a run to succeeded. An unresolved tool failure marks the run
failed; denied or uncertain execution marks it blocked. A successful retry of the
same operation resolves its failure, but a different successful read does not.
Failures and blocked runs pause the schedule for inspection.

## Time and permissions

- `--every 90m`: exactly 90 minutes, preserving its original phase. Intervals
  range from one minute to one year; they are never rounded to cron steps.
- `--in 20m`: one run after a delay.
- `--at 2026-10-01T09:00:00-05:00`: one run at an explicit RFC3339 timestamp.
- `--cron '0 9 * * MON-FRI' --timezone America/Chicago`: a five-field calendar
  schedule in an IANA timezone. The default is explicitly UTC. Weekdays use
  conventional cron numbering: 0/7 are Sunday, 1 is Monday. Restricted day-of-month
  and day-of-week fields match either condition. During daylight-saving changes,
  fixed clock-time jobs in a spring gap run at the first valid instant afterward;
  a repeated fall clock time runs once, at its first occurrence. Wildcard calendar
  jobs follow the available local clock times. The UI displays the next execution
  in UTC alongside the saved calendar timezone.

A machine that is asleep or off cannot execute local tasks. When the runner
returns, missed recurring occurrences coalesce into one run; a late one-time task
runs once. Scheduled time is an earliest start time, not a real-time guarantee.
Work waits behind an active execution. Ticks missed during that execution are
skipped on completion rather than queued as a backlog.

Jobs default to **read-only**, independently of the creating session's `--auto`.
Read-only jobs cannot execute shell commands, including test commands. To authorize
ordinary agent tools, including edits and shell commands, create a job explicitly:

```sh
builder -C /path/to/dedicated-checkout schedule add --every 2h \
  --allow-writes --rounds 30 --timeout 900 \
  'Run the test suite and investigate failures within this checkout'
```

This stores trust for that job; it is not an OS sandbox. Write-enabled jobs use
the saved checkout directly. Use a dedicated checkout if interactive work could
conflict with scheduled edits. Scheduled jobs are serialized with each other,
but do not lock other terminal or browser conversations out of the checkout.

The default budget is 50 model rounds and 30 minutes. An unauthorized tool request
marks the run **blocked** and pauses its schedule. Failures, deadlines, and
interruptions also pause it. Inspect the saved conversation before using
`schedule resume ID` to resume future recurring occurrences or `schedule run ID`
to queue one explicit execution. A completed or overdue one-time schedule can be
run manually; resuming it does not invent a new deadline.

`pause` and `delete` cancel queued work and future occurrences. An execution
already running finishes normally. Stop the daemon to interrupt its active run.
Deleted schedules retain their execution history, available by ID.

## Run after closing the terminal

Service generation prints a definition for review. It does not install anything,
copy credentials, or start a runner implicitly. The definition uses the current
Builder executable's absolute path; generate it from your installed binary.
Use the same `--home` or `BUILDER_HOME` setting when creating tasks and generating
the service. Default platform configuration/data locations remain separate.

### macOS: LaunchAgent

```sh
mkdir -p "$HOME/Library/LaunchAgents"
builder daemon --service launchd > "$HOME/Library/LaunchAgents/ai.builder.scheduler.plist"
plutil -lint "$HOME/Library/LaunchAgents/ai.builder.scheduler.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/ai.builder.scheduler.plist"
```

The agent starts at login and restarts if the runner exits. Its execution messages
are appended to `scheduler.log` in Builder's data directory; conversation output
stays in the journal. Rotate this log with your normal log-management tooling.
To stop the service:

```sh
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/ai.builder.scheduler.plist"
```

### Linux: systemd user service

```sh
mkdir -p "$HOME/.config/systemd/user"
builder daemon --service systemd > "$HOME/.config/systemd/user/builder-scheduler.service"
systemctl --user daemon-reload
systemctl --user enable --now builder-scheduler.service
journalctl --user -u builder-scheduler.service -f
```

User services normally follow the user's login lifecycle; configure lingering on
the host if execution must continue after logout. To stop it:

```sh
systemctl --user disable --now builder-scheduler.service
```

Service managers do not inherit an interactive shell's complete environment.
Configure any environment-backed model credentials, tool PATH, or model server
startup dependencies in the service manager. Builder does not embed secrets in
generated service definitions. The configured model endpoint must be reachable.

## Execution and recovery guarantees

The daemon owns an OS file lock for its process lifetime. Claiming an occurrence
and advancing its cadence happen in one SQLite transaction; unique indexes prevent
duplicate scheduled occurrences and overlapping pending executions of one job.
A session is linked durably before its first model request or tool execution.

On restart, an unfinished claim becomes **interrupted**, and its schedule pauses.
Builder does not replay it or infer success from a partial response. Existing
agent tool journals preserve ambiguous outcomes for inspection. This is not
exactly-once execution of external effects: a shell command can take effect before
its completion is committed.

Execution results are visible through schedule history, ordinary saved sessions,
and service logs. This release does not send desktop or external notifications,
attach scheduled turns to existing conversations, or expose scheduling in the
browser. Persistent scheduling is available through the CLI and terminal slash
commands.

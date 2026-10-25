//! Streaming model/tool rendering and RAII progress indicators.
use super::{nudge_line, print_todos, safe, stream, theme};
use crate::agent::{AgentEvent, SubagentActivity, SummaryStage};
use crate::presentation::{edit_focus_line, subagent_activity, subagent_round};
use builder_provider::{Activity, Event};
use console::style;
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub struct Renderer {
    interactive: bool,
    started: bool,
    spinner: Option<indicatif::ProgressBar>,
    pending: stream::StreamBuffer,
    started_at: std::time::Instant,
    bytes: usize,
    compacting: bool,
    /// The spinner slot currently holds the summarization progress bar.
    summary_bar: bool,
    heading_printed: bool,
    tool_started_at: Option<std::time::Instant>,
    tools: usize,
    reflow: stream::Reflow,
    prompt_tokens: usize,
    reasoning_bytes: usize,
    reasoning_started_at: Option<std::time::Instant>,
    model_activity: Option<ModelActivity>,
    /// Tools started and not yet finished; more than one means a parallel group.
    running: Vec<(String, String)>,
    /// Latest progress per running subagent, keyed by call ID.
    subagents: std::collections::BTreeMap<String, SubagentStatus>,
}

struct SubagentStatus {
    description: String,
    actions: usize,
    round: usize,
    reporting: bool,
    activity: SubagentActivity,
}

const NO_MODEL_ACTIVITY: u64 = u64::MAX;

#[derive(Clone)]
struct ModelActivity(Arc<AtomicU64>);

impl ModelActivity {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(NO_MODEL_ACTIVITY)))
    }

    fn record(&self, elapsed: std::time::Duration) {
        let millis = elapsed.as_millis().min(u128::from(u64::MAX - 1)) as u64;
        self.0.store(millis, Ordering::Relaxed);
    }

    fn label(&self, elapsed: std::time::Duration) -> String {
        let last = self.0.load(Ordering::Relaxed);
        if last == NO_MODEL_ACTIVITY {
            return "waiting for response".into();
        }
        let quiet = elapsed.as_millis().saturating_sub(u128::from(last));
        if quiet < 2_000 {
            "receiving now".into()
        } else {
            format!(
                "no data for {}",
                short_duration(
                    std::time::Duration::from_millis(quiet.min(u128::from(u64::MAX)) as u64)
                        .as_secs_f64()
                )
            )
        }
    }
}

impl Renderer {
    pub fn new(interactive: bool) -> Self {
        Self {
            interactive,
            started: false,
            spinner: None,
            pending: stream::StreamBuffer::default(),
            started_at: std::time::Instant::now(),
            bytes: 0,
            compacting: false,
            summary_bar: false,
            heading_printed: false,
            tool_started_at: None,
            tools: 0,
            reflow: stream::Reflow::default(),
            prompt_tokens: 0,
            reasoning_bytes: 0,
            reasoning_started_at: None,
            model_activity: None,
            running: Vec::new(),
            subagents: std::collections::BTreeMap::new(),
        }
    }
    pub fn event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Model(event) => self.model_event(event),
            AgentEvent::AutoCompact {
                messages,
                overhead,
                reserved,
                threshold,
                context_tokens,
            } => {
                self.finish_stream();
                self.clear_spinner();
                let total = messages + overhead + reserved;
                let percent = total * 100 / context_tokens.max(1);
                eprintln!(
                    "\n  {}",
                    theme::muted(&format!(
                        "Context {percent}% full · compacting {} conversation + {} tools + {} response reserve",
                        token_count(messages),
                        token_count(overhead),
                        token_count(reserved)
                    ))
                );
                debug_assert!(total >= threshold);
            }
            AgentEvent::Compacting {
                before,
                context_tokens,
            } => {
                self.finish_stream();
                self.clear_spinner();
                self.compacting = true;
                eprintln!(
                    "\n  {}",
                    theme::muted(&format!(
                        "Summarizing {} conversation · originals stay searchable · ctrl+c cancel",
                        token_count(before)
                    ))
                );
                let _ = context_tokens;
            }
            AgentEvent::Compacted {
                before,
                after,
                context_tokens,
            } => {
                self.clear_spinner();
                self.compacting = false;
                eprintln!(
                    "\n  {}",
                    theme::success(&format!(
                        "Context compacted · {} → {} · originals searchable",
                        token_count(before),
                        token_count(after)
                    ))
                );
                let _ = context_tokens;
            }
            AgentEvent::CompactionProgress {
                fraction,
                fragment,
                stage,
            } => {
                if !self.interactive {
                    return;
                }
                if !self.summary_bar {
                    self.clear_spinner();
                    self.start_summary_bar();
                }
                let part = if fragment > 1 {
                    format!("part {fragment} · ")
                } else {
                    String::new()
                };
                let detail = match stage {
                    SummaryStage::Waiting => "waiting for the model".to_owned(),
                    SummaryStage::Reading { processed, total } => format!(
                        "reading {} of {} prompt tokens",
                        compact_count(processed),
                        compact_count(total)
                    ),
                    // Reasoning before any handoff text is not yet writing.
                    SummaryStage::Writing {
                        summary_bytes: 0,
                        reasoning_bytes,
                        ..
                    } => format!(
                        "thinking · {} reasoning",
                        crate::input::buffer::size(reasoning_bytes)
                    ),
                    SummaryStage::Writing {
                        summary_bytes,
                        expected,
                        ..
                    } => format!(
                        "writing the summary · {} of ~{}",
                        crate::input::buffer::size(summary_bytes),
                        crate::input::buffer::size(expected)
                    ),
                };
                if let Some(bar) = &self.spinner {
                    bar.set_position((fraction.clamp(0.0, 1.0) * 1000.0) as u64);
                    bar.set_message(format!("{part}{detail}"));
                }
            }
            AgentEvent::SummaryRecovery { size, limit } => {
                self.finish_stream();
                self.clear_spinner();
                eprintln!(
                    "\n  {}",
                    theme::warning(&format!(
                        "Tightening context summary · {} exceeded the {} target",
                        token_count(size),
                        token_count(limit)
                    ))
                );
            }
            AgentEvent::MemoryNotice(note) => {
                self.finish_stream();
                self.clear_spinner();
                eprintln!("\n  {}", safe(&note));
            }
            AgentEvent::ExplorationRecovery { calls } => {
                self.finish_stream();
                self.clear_spinner();
                eprintln!(
                    "\n  {}",
                    theme::muted(&format!(
                        "Recovering after {calls} failed actions · context preserved"
                    ))
                );
            }
            AgentEvent::ProgressNudge {
                calls,
                step,
                repeated_reads,
                planning,
            } => {
                self.finish_stream();
                self.clear_spinner();
                eprintln!(
                    "\n  {}",
                    theme::warning(&nudge_line(calls, step, repeated_reads, planning))
                );
            }
            AgentEvent::EditFocus { calls, step, reads } => {
                self.finish_stream();
                self.clear_spinner();
                eprintln!(
                    "\n  {}",
                    theme::warning(&edit_focus_line(calls, step, reads))
                );
            }
            AgentEvent::OutputRecovery { budget } => {
                self.finish_stream();
                self.clear_spinner();
                let operation = if self.compacting {
                    "Context summary"
                } else {
                    "Response"
                };
                eprintln!(
                    "\n  {}",
                    theme::warning(&format!(
                        "{operation} needed more room · retrying with {} · incomplete draft discarded",
                        token_count(budget)
                    ))
                );
            }
            AgentEvent::RepetitionNotice { name, count, limit } => {
                self.finish_stream();
                self.clear_spinner();
                eprintln!(
                    "\n  {}",
                    theme::warning(&format!(
                        "↺ the model has repeated the same {} call {count} times · asking it to change approach · stopped at {limit}",
                        tool_label(&name).to_lowercase()
                    ))
                );
            }
            AgentEvent::ToolStarted { name, detail } => {
                self.finish_stream();
                self.clear_spinner();
                if self.interactive {
                    if self.started {
                        println!();
                        self.started = false;
                        self.reflow.reset();
                    }
                    if self.running.is_empty() {
                        self.tool_started_at = Some(std::time::Instant::now());
                    }
                    self.running.push((name, detail));
                    self.start_running_spinner();
                }
            }
            AgentEvent::ToolFinished {
                name,
                detail,
                note,
                failed,
            } => {
                if self.interactive {
                    self.clear_spinner();
                    self.tools += 1;
                    if let Some(index) = self.running.iter().position(|(n, _)| *n == name) {
                        self.running.remove(index);
                    }
                    // Every row of a parallel group reports the group's time.
                    let elapsed = self
                        .tool_started_at
                        .map_or(0.0, |time| time.elapsed().as_secs_f64());
                    if self.running.is_empty() {
                        self.tool_started_at = None;
                        self.subagents.clear();
                    }
                    // A recorded todo list is shown as its board instead.
                    if failed || name != builder_core::todo::TOOL {
                        self.print_tool_result(&name, &detail, note.as_deref(), failed, elapsed);
                    }
                    // The rest of a parallel group, often slow subagents, keeps
                    // a live status line.
                    if !self.running.is_empty() {
                        self.start_running_spinner();
                    }
                }
            }
            AgentEvent::SubagentProgress {
                call_id,
                description,
                actions,
                round,
                reporting,
                activity,
            } => {
                let live = activity.is_live();
                self.subagents.insert(
                    call_id,
                    SubagentStatus {
                        description,
                        actions,
                        round,
                        reporting,
                        activity,
                    },
                );
                if live {
                    self.record_model_activity();
                }
                let message = self.running_message();
                if let Some(spinner) = &self.spinner {
                    spinner.set_message(self.fit(&message, 14));
                }
            }
            AgentEvent::TodosUpdated(list) => {
                if self.interactive {
                    self.finish_stream();
                    self.clear_spinner();
                    print_todos(&list);
                }
            }
        }
    }
    /// The status line for running tools. A redraw keeps the group's elapsed
    /// time, and while a subagent runs it says whether its model is sending.
    fn start_running_spinner(&mut self) {
        self.clear_spinner();
        let message = self.running_message();
        let subagent = self
            .running
            .iter()
            .any(|(name, _)| name == builder_tools::SUBAGENT_TOOL);
        self.start_spinner(message, subagent);
        if let Some(started) = self.tool_started_at {
            self.spinner = self
                .spinner
                .take()
                .map(|spinner| spinner.with_elapsed(started.elapsed()));
        }
    }
    /// One spinner line for whatever is running: a single tool, a parallel
    /// group, or subagents with their latest activity.
    fn running_message(&self) -> String {
        let subagents = self
            .running
            .iter()
            .filter(|(name, _)| name == builder_tools::SUBAGENT_TOOL)
            .count();
        let latest = self.subagents.values().max_by_key(|status| status.actions);
        match (self.running.as_slice(), latest) {
            ([(name, detail)], None) => {
                let label = tool_label(name);
                match self.fit(detail, label.len() + 36) {
                    detail if detail.is_empty() => format!("{label} · ctrl+c cancel"),
                    detail => format!("{label}  {detail} · ctrl+c cancel"),
                }
            }
            ([_], Some(status)) => format!(
                "Subagent  {} · {} · {} actions · {} · ctrl+c cancel",
                status.description,
                subagent_round(status.round, status.reporting),
                status.actions,
                subagent_activity(&status.activity)
            ),
            (running, Some(status)) => format!(
                "{} running ({subagents} subagents, {} actions) · {}: {} · ctrl+c cancel",
                running.len(),
                self.subagents.values().map(|s| s.actions).sum::<usize>(),
                status.description,
                subagent_activity(&status.activity)
            ),
            (running, None) => format!(
                "Running {} actions in parallel · ctrl+c cancel",
                running.len()
            ),
        }
    }
    /// The current request size, in the same estimated tokens the agent budgets
    /// with. Silence during a long prompt is expected work, not a hang.
    fn prompt_size(&self) -> String {
        match self.prompt_tokens {
            0 => "the conversation".to_owned(),
            tokens if tokens < 1000 => format!("a ~{tokens}-token prompt"),
            tokens => format!("a ~{}k-token prompt", tokens / 1000),
        }
    }
    /// Clip untrusted tool text to the remaining terminal width.
    fn fit(&self, text: &str, used: usize) -> String {
        let width = (console::Term::stderr().size().1 as usize).saturating_sub(used);
        crate::input::layout::ellipsize(&safe(text), width.clamp(12, 88))
    }
    fn print_tool_result(
        &self,
        name: &str,
        detail: &str,
        note: Option<&str>,
        failed: bool,
        elapsed: f64,
    ) {
        const LABEL_WIDTH: usize = 12;
        let width = (console::Term::stderr().size().1 as usize).clamp(20, 100);
        let label = crate::input::layout::ellipsize(&tool_label(name), LABEL_WIDTH);
        let label = format!(
            "{}{}",
            label,
            " ".repeat(LABEL_WIDTH.saturating_sub(display_width(&label)))
        );
        let exit_failed = name == "shell" && note.is_some_and(|note| note.starts_with("exit "));
        let warning = failed || exit_failed;
        let icon = if warning {
            theme::warning("!")
        } else {
            theme::success("✓")
        };
        let inline_note = note.filter(|note| !failed && display_width(note) <= 28);
        let mut metadata = inline_note
            .map(str::to_owned)
            .into_iter()
            .collect::<Vec<_>>();
        if elapsed >= 0.1 {
            metadata.push(short_duration(elapsed));
        }
        let metadata = metadata.join(" · ");
        let fixed = 4 + 2 + LABEL_WIDTH;
        let metadata_space = usize::from(!metadata.is_empty()) * 2;
        let detail_width = width
            .saturating_sub(fixed + display_width(&metadata) + metadata_space)
            .max(1);
        let detail = crate::input::layout::ellipsize(&safe(detail), detail_width);

        if width >= 52 {
            let gap = if metadata.is_empty() {
                0
            } else {
                width
                    .saturating_sub(fixed + display_width(&detail) + display_width(&metadata))
                    .max(2)
            };
            eprintln!(
                "    {icon} {label}{detail}{}{}",
                " ".repeat(gap),
                theme::muted(&metadata)
            );
        } else {
            eprintln!("    {icon} {}  {detail}", label.trim_end());
            if !metadata.is_empty() {
                eprintln!("        {}", theme::muted(&metadata));
            }
        }

        if let Some(note) = note.filter(|note| Some(*note) != inline_note) {
            for line in crate::input::layout::wrap(&safe(note), width.saturating_sub(8).max(12)) {
                eprintln!("        {}", theme::muted(&line));
            }
        }
    }
    fn clear_spinner(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.finish_and_clear();
        }
        self.model_activity = None;
        self.summary_bar = false;
    }
    fn start_summary_bar(&mut self) {
        let bar = indicatif::ProgressBar::new(1000);
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "  {spinner:.cyan} Summarizing {bar:28.117/238} {percent:>3}%  {msg:.dim} · {elapsed:.dim} · ctrl+c cancel",
            )
            .expect("static progress template")
            .progress_chars("━╸─")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        self.spinner = Some(bar);
        self.summary_bar = true;
    }
    fn start_spinner(&mut self, message: String, tracks_activity: bool) {
        let spinner = indicatif::ProgressBar::new_spinner();
        let style = if tracks_activity {
            let activity = ModelActivity::new();
            let display = activity.clone();
            self.model_activity = Some(activity);
            indicatif::ProgressStyle::with_template(
                "  {spinner:.cyan} {msg}  {elapsed:.dim} · {activity:.dim}",
            )
            .expect("static progress template")
            .with_key(
                "activity",
                move |state: &indicatif::ProgressState, writer: &mut dyn std::fmt::Write| {
                    let _ = writer.write_str(&display.label(state.elapsed()));
                },
            )
        } else {
            self.model_activity = None;
            indicatif::ProgressStyle::with_template("  {spinner:.cyan} {msg}  {elapsed:.dim}")
                .expect("static progress template")
        };
        spinner.set_style(style.tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]));
        spinner.set_message(self.fit(&message, 14));
        spinner.enable_steady_tick(std::time::Duration::from_millis(120));
        self.spinner = Some(spinner);
    }
    fn record_model_activity(&self) {
        if let (Some(activity), Some(spinner)) = (&self.model_activity, &self.spinner) {
            activity.record(spinner.elapsed());
            spinner.tick();
        }
    }
    fn model_event(&mut self, event: Event) {
        match event {
            // The estimator the agent budgets with, so the waiting states and
            // the compaction notice quote the same number.
            Event::Prompt { bytes } => self.prompt_tokens = bytes.div_ceil(2),
            Event::PromptProgress { processed, total } => {
                self.record_model_activity();
                if !self.compacting
                    && let Some(spinner) = &self.spinner
                {
                    spinner.set_message(self.fit(
                        &format!(
                            "Reading {} of {} prompt tokens · ctrl+c cancel",
                            compact_count(processed),
                            compact_count(total)
                        ),
                        14,
                    ));
                }
            }
            Event::Activity(activity) => {
                if !matches!(activity, Activity::Connected) {
                    self.record_model_activity();
                }
                if self.summary_bar {
                    return;
                }
                let size = self.prompt_size();
                if let Some(spinner) = &self.spinner {
                    let message = if self.compacting {
                        "Summarizing context · originals retained · ctrl+c cancel".to_owned()
                    } else {
                        match activity {
                            Activity::Connected => {
                                format!("Waiting on {size} · ctrl+c cancel")
                            }
                            Activity::Thinking => {
                                self.reasoning_bytes = 0;
                                self.reasoning_started_at = Some(std::time::Instant::now());
                                "Thinking · ctrl+c cancel".to_owned()
                            }
                            Activity::PreparingTools => {
                                "Preparing actions · ctrl+c cancel".to_owned()
                            }
                        }
                    };
                    spinner.set_message(self.fit(&message, 14));
                }
            }
            Event::Attempt { number, maximum } => {
                self.finish_stream();
                self.started = false;
                self.reflow.reset();
                self.pending.reset();
                self.reasoning_bytes = 0;
                self.reasoning_started_at = None;
                // The summarization bar stays in place across fragment requests.
                if self.interactive && self.summary_bar {
                    return;
                }
                if self.interactive {
                    self.clear_spinner();
                    if !self.heading_printed || number > 1 {
                        eprintln!(
                            "\n  {}",
                            theme::accent(&if number == 1 {
                                "Builder".to_owned()
                            } else {
                                format!("Builder · attempt {number}/{maximum}")
                            })
                        );
                        self.heading_printed = true;
                    }
                    let size = self.prompt_size();
                    self.start_spinner(format!("Sending {size} · ctrl+c cancel"), true);
                }
            }
            Event::Delta(text) => {
                // Headless stdout receives only the final committed response.
                if self.interactive {
                    self.clear_spinner();
                    self.bytes += text.len();
                    self.pending.push(&text);
                    if self.pending.len() >= 8192 {
                        self.flush();
                    }
                }
            }
            Event::Reasoning(text) => {
                self.record_model_activity();
                self.reasoning_bytes += text.len();
                if self.reasoning_started_at.is_none() {
                    self.reasoning_started_at = Some(std::time::Instant::now());
                }
                if !self.interactive || self.compacting || self.summary_bar {
                    return;
                }
                let tokens = (self.reasoning_bytes / 4).max(1);
                let message = if tokens < 50 {
                    "Thinking · ctrl+c cancel".to_owned()
                } else {
                    let elapsed = self
                        .reasoning_started_at
                        .map_or(0.0, |t| t.elapsed().as_secs_f64());
                    if elapsed >= 1.0 {
                        let rate = (tokens as f64 / elapsed).round() as u64;
                        format!(
                            "Thinking · {} tokens · {rate} tok/s · ctrl+c cancel",
                            compact_count(tokens as u64)
                        )
                    } else {
                        format!(
                            "Thinking · {} tokens · ctrl+c cancel",
                            compact_count(tokens as u64)
                        )
                    }
                };
                if self.spinner.is_none() {
                    self.finish_stream();
                    if self.started {
                        println!();
                        self.started = false;
                        self.reflow.reset();
                    }
                    self.start_spinner(message, true);
                } else if let Some(spinner) = &self.spinner {
                    spinner.set_message(self.fit(&message, 14));
                }
            }
            Event::ToolProgress { name, calls, bytes } => {
                if !self.interactive || self.compacting || self.summary_bar {
                    return;
                }
                let action = if name.is_empty() {
                    "an action".to_owned()
                } else {
                    tool_label(&name)
                };
                let batch = if calls > 1 {
                    format!(" (action {calls})")
                } else {
                    String::new()
                };
                let message = format!(
                    "Preparing {action}{batch} · {} chars written · ctrl+c cancel",
                    compact_count(bytes as u64)
                );
                if self.spinner.is_none() {
                    // Streamed prose cleared the spinner. A large edit can take
                    // minutes to generate, so show that the response continues.
                    self.finish_stream();
                    if self.started {
                        println!();
                        self.started = false;
                        self.reflow.reset();
                    }
                    self.start_spinner(message, true);
                } else if let Some(spinner) = &self.spinner {
                    spinner.set_message(self.fit(&message, 14));
                }
                self.record_model_activity();
            }
            Event::Retry { delay_ms, reason } => {
                self.finish_stream();
                self.clear_spinner();
                eprintln!("\n  {} {}", style("↻ reconnecting").yellow(), safe(&reason));
                eprintln!(
                    "  {}",
                    style(format!(
                        "Retry in {:.1}s · partial output discarded · conversation preserved",
                        delay_ms as f64 / 1000.0
                    ))
                    .dim()
                );
            }
        }
    }
    pub fn flush(&mut self) {
        let width = (console::Term::stdout().size().1 as usize)
            .saturating_sub(4)
            .clamp(12, 96);
        let mut text = if self.pending.is_empty() {
            String::new()
        } else {
            let pending = self.pending.take();
            self.reflow.push(&pending, width, "    ")
        };
        text.push_str(&self.reflow.flush_partial(width, "    "));
        self.write_stream(&text);
    }
    fn finish_stream(&mut self) {
        self.flush();
        let width = (console::Term::stdout().size().1 as usize)
            .saturating_sub(4)
            .clamp(12, 96);
        let text = self.reflow.finish(width, "    ");
        self.write_stream(&text);
    }
    fn write_stream(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut out = io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
        self.started = true;
    }
    pub fn finish(&mut self) {
        self.finish_stream();
        self.clear_spinner();
        if self.started {
            println!();
        }
        if self.interactive && self.bytes > 0 {
            eprintln!(
                "\n  {}\n",
                theme::muted(&format!(
                    "{}{}",
                    long_duration(self.started_at.elapsed().as_secs_f64()),
                    if self.tools == 0 {
                        String::new()
                    } else {
                        format!(" · {} actions", self.tools)
                    }
                ))
            );
        }
    }
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

pub fn token_count(tokens: usize) -> String {
    if tokens < 1000 {
        format!("{tokens} tokens")
    } else {
        let value = tokens as f64 / 1000.0;
        format!("{value:.1}k tokens")
    }
}

fn compact_count(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{:.1}k", count as f64 / 1000.0)
    }
}

fn short_duration(seconds: f64) -> String {
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else if seconds < 60.0 {
        format!("{seconds:.0}s")
    } else {
        long_duration(seconds)
    }
}

fn long_duration(seconds: f64) -> String {
    if seconds < 1.0 {
        return "<1s".into();
    }
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let seconds = seconds.round() as u64;
    match seconds {
        0..=3599 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}
fn tool_label(name: &str) -> String {
    match name {
        "read_file" => "Read file".into(),
        "edit_file" | "multi_edit" => "Edit file".into(),
        builder_tools::SUBAGENT_TOOL => "Subagent".into(),
        "write_file" => "Write file".into(),
        "list_files" => "List files".into(),
        "search" => "Search".into(),
        "shell" => "Run command".into(),
        builder_core::todo::TOOL => "Todo".into(),
        _ => safe(name).replace('_', " "),
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.clear_spinner();
    }
}
#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn reasoning_reports_live_data_without_resetting_total_elapsed_time() {
        let mut renderer = Renderer::new(false);
        renderer.start_spinner("Sending".into(), true);
        renderer
            .spinner
            .as_ref()
            .unwrap()
            .set_draw_target(indicatif::ProgressDrawTarget::hidden());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let elapsed = renderer.spinner.as_ref().unwrap().elapsed();
        renderer.model_event(Event::Activity(Activity::Thinking));
        renderer.model_event(Event::Reasoning("streamed fragment".into()));
        assert!(renderer.spinner.as_ref().unwrap().elapsed() >= elapsed);
        let current = renderer.spinner.as_ref().unwrap().elapsed();
        assert_eq!(
            renderer.model_activity.as_ref().unwrap().label(current),
            "receiving now"
        );
        renderer.model_event(Event::Activity(Activity::PreparingTools));
        assert!(renderer.spinner.as_ref().unwrap().elapsed() >= elapsed);
    }

    #[test]
    fn reasoning_tracks_progress_and_token_count() {
        let mut renderer = Renderer::new(true);
        renderer.start_spinner("Sending".into(), true);
        renderer
            .spinner
            .as_ref()
            .unwrap()
            .set_draw_target(indicatif::ProgressDrawTarget::hidden());
        renderer.model_event(Event::Activity(Activity::Thinking));
        // Small reasoning under 50 tokens keeps the base message
        renderer.model_event(Event::Reasoning("short thinking".into()));
        assert_eq!(
            renderer.spinner.as_ref().unwrap().message(),
            "Thinking · ctrl+c cancel"
        );
        // Accumulated reasoning over 50 tokens (>= 200 bytes) reports token count
        renderer.model_event(Event::Reasoning("a".repeat(400)));
        let message = renderer.spinner.as_ref().unwrap().message();
        assert!(message.starts_with("Thinking · 103 tokens"));
    }

    #[test]
    fn activity_label_distinguishes_waiting_receiving_and_silence() {
        let activity = ModelActivity::new();
        assert_eq!(
            activity.label(std::time::Duration::from_secs(10)),
            "waiting for response"
        );
        activity.record(std::time::Duration::from_secs(10));
        assert_eq!(
            activity.label(std::time::Duration::from_secs(11)),
            "receiving now"
        );
        assert_eq!(
            activity.label(std::time::Duration::from_secs(52)),
            "no data for 42s"
        );
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;
    use builder_core::todo::{Item, List, Status};

    fn hide(renderer: &Renderer) {
        if let Some(spinner) = &renderer.spinner {
            spinner.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        }
    }

    fn running(name: &str, detail: &str) -> (String, String) {
        (name.into(), detail.into())
    }

    #[test]
    fn formatting_helpers_round_counts_durations_and_labels() {
        assert_eq!(token_count(999), "999 tokens");
        assert_eq!(token_count(1000), "1.0k tokens");
        assert_eq!(token_count(131_072), "131.1k tokens");
        assert_eq!(compact_count(42), "42");
        assert_eq!(compact_count(12_345), "12.3k");
        assert_eq!(short_duration(0.04), "0.0s");
        assert_eq!(short_duration(2.26), "2.3s");
        assert_eq!(short_duration(12.6), "13s");
        assert_eq!(short_duration(60.0), "1m 00s");
        assert_eq!(long_duration(0.5), "<1s");
        assert_eq!(long_duration(5.56), "5.6s");
        assert_eq!(long_duration(61.0), "1m 01s");
        assert_eq!(long_duration(3599.4), "59m 59s");
        assert_eq!(long_duration(3600.0), "1h 00m");
        assert_eq!(long_duration(7384.0), "2h 03m");
        assert_eq!(tool_label("read_file"), "Read file");
        assert_eq!(tool_label("edit_file"), "Edit file");
        assert_eq!(tool_label("multi_edit"), "Edit file");
        assert_eq!(tool_label("subagent"), "Subagent");
        assert_eq!(tool_label("write_file"), "Write file");
        assert_eq!(tool_label("list_files"), "List files");
        assert_eq!(tool_label("search"), "Search");
        assert_eq!(tool_label("shell"), "Run command");
        assert_eq!(tool_label("todo_write"), "Todo");
        assert_eq!(tool_label("code_search"), "code search");
        assert_eq!(tool_label("evil\x1b[31m_name"), "evil name");
        assert_eq!(display_width("ab"), 2);
        assert_eq!(display_width("日本"), 4);
    }

    #[test]
    fn prompt_size_and_running_message_describe_the_current_work() {
        let mut renderer = Renderer::new(true);
        assert_eq!(renderer.prompt_size(), "the conversation");
        renderer.model_event(Event::Prompt { bytes: 1500 });
        assert_eq!(renderer.prompt_size(), "a ~750-token prompt");
        renderer.model_event(Event::Prompt { bytes: 300_000 });
        assert_eq!(renderer.prompt_size(), "a ~150k-token prompt");

        renderer.running = vec![running("read_file", "src/main.rs")];
        assert_eq!(
            renderer.running_message(),
            "Read file  src/main.rs · ctrl+c cancel"
        );
        renderer.running = vec![running("shell", "")];
        assert_eq!(renderer.running_message(), "Run command · ctrl+c cancel");
        renderer.running = vec![running("read_file", "a"), running("search", "b")];
        assert_eq!(
            renderer.running_message(),
            "Running 2 actions in parallel · ctrl+c cancel"
        );
        renderer.running = vec![running("subagent", "map")];
        renderer.subagents.insert(
            "c1".into(),
            SubagentStatus {
                description: "map DI".into(),
                actions: 3,
                round: 2,
                reporting: false,
                activity: SubagentActivity::Thinking,
            },
        );
        assert_eq!(
            renderer.running_message(),
            "Subagent  map DI · round 2 · 3 actions · thinking · ctrl+c cancel"
        );
        renderer.running.push(running("read_file", "x"));
        renderer.subagents.insert(
            "c2".into(),
            SubagentStatus {
                description: "trace".into(),
                actions: 5,
                round: 1,
                reporting: true,
                activity: SubagentActivity::Waiting,
            },
        );
        assert_eq!(
            renderer.running_message(),
            "2 running (1 subagents, 8 actions) · trace: waiting for the model · ctrl+c cancel"
        );
    }

    #[test]
    fn interactive_events_drive_spinners_tool_rows_and_subagent_status() {
        let mut renderer = Renderer::new(true);
        let model = |event| AgentEvent::Model(event);
        renderer.event(model(Event::Attempt {
            number: 1,
            maximum: 3,
        }));
        hide(&renderer);
        assert!(renderer.spinner.is_some());
        assert!(renderer.heading_printed);
        assert!(renderer.model_activity.is_some());
        renderer.event(model(Event::Activity(Activity::Connected)));
        renderer.event(model(Event::PromptProgress {
            processed: 10,
            total: 2000,
        }));
        renderer.event(model(Event::Activity(Activity::Thinking)));
        renderer.event(model(Event::Reasoning("hmm".into())));
        renderer.event(model(Event::Activity(Activity::PreparingTools)));
        renderer.event(model(Event::ToolProgress {
            name: "edit_file".into(),
            calls: 2,
            bytes: 1500,
        }));
        renderer.event(model(Event::ToolProgress {
            name: String::new(),
            calls: 1,
            bytes: 10,
        }));

        renderer.event(AgentEvent::ToolStarted {
            name: "read_file".into(),
            detail: "a.rs".into(),
        });
        hide(&renderer);
        assert_eq!(renderer.running.len(), 1);
        assert!(renderer.tool_started_at.is_some());
        renderer.event(AgentEvent::ToolStarted {
            name: "subagent".into(),
            detail: "map".into(),
        });
        hide(&renderer);
        assert_eq!(renderer.running.len(), 2);
        assert!(
            renderer.model_activity.is_some(),
            "a running subagent tracks its model activity"
        );
        renderer.event(AgentEvent::SubagentProgress {
            call_id: "c1".into(),
            description: "map".into(),
            actions: 1,
            round: 1,
            reporting: false,
            activity: SubagentActivity::Reading {
                processed: 1,
                total: 2,
            },
        });
        assert_eq!(renderer.subagents.len(), 1);
        renderer.event(AgentEvent::ToolFinished {
            name: "read_file".into(),
            detail: "a.rs".into(),
            note: Some("12 lines".into()),
            failed: false,
        });
        hide(&renderer);
        assert_eq!(renderer.tools, 1);
        assert_eq!(renderer.running.len(), 1);
        assert!(
            renderer.spinner.is_some(),
            "the remaining subagent keeps a status line"
        );
        renderer.event(AgentEvent::ToolFinished {
            name: "subagent".into(),
            detail: "map".into(),
            note: Some("report · 44 tool calls\nsecond line of the note".into()),
            failed: false,
        });
        assert!(renderer.running.is_empty());
        assert!(renderer.subagents.is_empty());
        assert!(renderer.tool_started_at.is_none());
        assert!(renderer.spinner.is_none());

        renderer.event(AgentEvent::ToolStarted {
            name: "shell".into(),
            detail: "cargo test".into(),
        });
        hide(&renderer);
        renderer.event(AgentEvent::ToolFinished {
            name: "shell".into(),
            detail: "cargo test".into(),
            note: Some("exit 101".into()),
            failed: false,
        });
        renderer.event(AgentEvent::ToolStarted {
            name: "edit_file".into(),
            detail: "a.rs".into(),
        });
        hide(&renderer);
        renderer.event(AgentEvent::ToolFinished {
            name: "edit_file".into(),
            detail: "a.rs".into(),
            note: Some("ERROR: no match".into()),
            failed: true,
        });
        assert_eq!(renderer.tools, 4);

        let list = List {
            items: vec![Item {
                content: "step".into(),
                status: Status::InProgress,
            }],
        };
        renderer.event(AgentEvent::ToolStarted {
            name: "todo_write".into(),
            detail: "1 item".into(),
        });
        hide(&renderer);
        renderer.event(AgentEvent::ToolFinished {
            name: "todo_write".into(),
            detail: "1 item".into(),
            note: None,
            failed: false,
        });
        renderer.event(AgentEvent::TodosUpdated(list));

        renderer.event(model(Event::Attempt {
            number: 2,
            maximum: 3,
        }));
        hide(&renderer);
        renderer.event(model(Event::Delta("Hello ".into())));
        assert!(renderer.spinner.is_none(), "prose clears the spinner");
        renderer.event(model(Event::Delta("world ".repeat(2000))));
        assert!(renderer.bytes > 8192);
        renderer.event(model(Event::ToolProgress {
            name: "write_file".into(),
            calls: 1,
            bytes: 5,
        }));
        hide(&renderer);
        assert!(
            renderer.spinner.is_some(),
            "a long edit after prose shows the response continues"
        );
        renderer.event(model(Event::Retry {
            delay_ms: 1500,
            reason: "connection reset".into(),
        }));
        assert!(renderer.spinner.is_none());

        renderer.event(AgentEvent::ExplorationRecovery { calls: 3 });
        renderer.event(AgentEvent::ProgressNudge {
            calls: 12,
            step: Some((2, 5)),
            repeated_reads: 1,
            planning: false,
        });
        renderer.event(AgentEvent::EditFocus {
            calls: 16,
            step: (2, 5),
            reads: true,
        });
        renderer.event(AgentEvent::RepetitionNotice {
            name: "shell".into(),
            count: 3,
            limit: 4,
        });
        renderer.event(AgentEvent::MemoryNotice(
            "memory \x1b[31munavailable".into(),
        ));
        renderer.event(AgentEvent::OutputRecovery { budget: 8192 });
        renderer.event(AgentEvent::SummaryRecovery {
            size: 20_000,
            limit: 12_000,
        });

        renderer.event(AgentEvent::AutoCompact {
            messages: 70_000,
            overhead: 8_000,
            reserved: 4_000,
            threshold: 75_000,
            context_tokens: 100_000,
        });
        renderer.event(AgentEvent::Compacting {
            before: 70_000,
            context_tokens: 100_000,
        });
        assert!(renderer.compacting);
        renderer.event(AgentEvent::OutputRecovery { budget: 8192 });
        renderer.event(AgentEvent::CompactionProgress {
            fraction: 0.0,
            fragment: 1,
            stage: SummaryStage::Waiting,
        });
        hide(&renderer);
        assert!(renderer.summary_bar);
        assert!(renderer.spinner.is_some());
        renderer.event(model(Event::Attempt {
            number: 1,
            maximum: 3,
        }));
        assert!(renderer.summary_bar, "the bar survives fragment requests");
        renderer.event(model(Event::Activity(Activity::Thinking)));
        renderer.event(model(Event::PromptProgress {
            processed: 5,
            total: 10,
        }));
        renderer.event(model(Event::ToolProgress {
            name: "x".into(),
            calls: 1,
            bytes: 1,
        }));
        renderer.event(AgentEvent::CompactionProgress {
            fraction: 0.25,
            fragment: 2,
            stage: SummaryStage::Reading {
                processed: 500,
                total: 4000,
            },
        });
        renderer.event(AgentEvent::CompactionProgress {
            fraction: 0.5,
            fragment: 2,
            stage: SummaryStage::Writing {
                summary_bytes: 0,
                reasoning_bytes: 300,
                expected: 4000,
            },
        });
        renderer.event(AgentEvent::CompactionProgress {
            fraction: 0.9,
            fragment: 2,
            stage: SummaryStage::Writing {
                summary_bytes: 3000,
                reasoning_bytes: 300,
                expected: 4000,
            },
        });
        assert_eq!(renderer.spinner.as_ref().unwrap().position(), 900);
        renderer.event(AgentEvent::Compacted {
            before: 70_000,
            after: 20_000,
            context_tokens: 100_000,
        });
        assert!(!renderer.compacting);
        assert!(!renderer.summary_bar);
        assert!(renderer.spinner.is_none());
        renderer.finish();
        assert!(renderer.spinner.is_none());
    }

    #[test]
    fn a_headless_renderer_never_draws_progress_or_streams_prose() {
        let mut renderer = Renderer::new(false);
        let events = vec![
            AgentEvent::Model(Event::Attempt {
                number: 1,
                maximum: 3,
            }),
            AgentEvent::Model(Event::Activity(Activity::Thinking)),
            AgentEvent::Model(Event::Delta("streamed".into())),
            AgentEvent::Model(Event::ToolProgress {
                name: "edit_file".into(),
                calls: 1,
                bytes: 10,
            }),
            AgentEvent::ToolStarted {
                name: "read_file".into(),
                detail: "a.rs".into(),
            },
            AgentEvent::ToolFinished {
                name: "read_file".into(),
                detail: "a.rs".into(),
                note: None,
                failed: false,
            },
            AgentEvent::SubagentProgress {
                call_id: "c1".into(),
                description: "map".into(),
                actions: 1,
                round: 1,
                reporting: true,
                activity: SubagentActivity::Writing,
            },
            AgentEvent::TodosUpdated(List::default()),
            AgentEvent::CompactionProgress {
                fraction: 0.5,
                fragment: 1,
                stage: SummaryStage::Waiting,
            },
            AgentEvent::Compacting {
                before: 1,
                context_tokens: 2,
            },
            AgentEvent::Compacted {
                before: 1,
                after: 1,
                context_tokens: 2,
            },
        ];
        for event in events {
            renderer.event(event);
            assert!(renderer.spinner.is_none());
        }
        assert!(renderer.running.is_empty());
        assert_eq!(renderer.tools, 0);
        assert_eq!(
            renderer.bytes, 0,
            "headless stdout gets only the final answer"
        );
        assert!(!renderer.summary_bar);
        renderer.finish();
    }
}

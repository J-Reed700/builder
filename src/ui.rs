use crate::agent::AgentEvent;
use builder_provider::{Activity, Event};
use builder_tools::Action;
use console::style;
use std::io::{self, IsTerminal, Write};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub mod stream;
pub mod theme;

pub fn safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    stream::Sanitizer::default().push(text, &mut out);
    out
}

pub fn banner(profile: &str, model: &str, workspace: &std::path::Path, session: &str, mode: &str) {
    let width = (console::Term::stderr().size().1 as usize)
        .saturating_sub(4)
        .clamp(1, 76);
    let fit = |text: &str| crate::input::layout::clip(&safe(text), width);
    eprintln!(
        "\n  {}  {}",
        theme::accent("builder"),
        theme::muted(env!("CARGO_PKG_VERSION"))
    );
    let profile = if profile.is_empty() { model } else { profile };
    let session: String = session.chars().take(8).collect();
    let workspace = std::env::var_os("HOME")
        .and_then(|home| workspace.strip_prefix(home).ok())
        .map_or_else(
            || workspace.display().to_string(),
            |path| format!("~/{}", path.display()),
        );
    eprintln!("  {}", theme::title(&fit(&workspace)));
    eprintln!("  {}", theme::muted(&fit(&format!("{profile} · {mode}"))));
    eprintln!("  {}\n", theme::muted(&fit(&format!("session {session}"))));
}
pub struct Renderer {
    interactive: bool,
    started: bool,
    spinner: Option<indicatif::ProgressBar>,
    pending: stream::StreamBuffer,
    started_at: std::time::Instant,
    bytes: usize,
    compacting: bool,
    heading_printed: bool,
    tool_started_at: Option<std::time::Instant>,
    tools: usize,
    reflow: stream::Reflow,
    prompt_tokens: usize,
    model_activity: Option<ModelActivity>,
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
            heading_printed: false,
            tool_started_at: None,
            tools: 0,
            reflow: stream::Reflow::default(),
            prompt_tokens: 0,
            model_activity: None,
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
                        "Refocusing after {calls} actions without a file change · context preserved"
                    ))
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
                    self.tool_started_at = Some(std::time::Instant::now());
                    let label = tool_label(&name);
                    self.start_spinner(
                        match self.fit(&detail, label.len() + 36) {
                            detail if detail.is_empty() => format!("{label} · ctrl+c cancel"),
                            detail => format!("{label}  {detail} · ctrl+c cancel"),
                        },
                        false,
                    );
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
                    let elapsed = self
                        .tool_started_at
                        .take()
                        .map_or(0.0, |time| time.elapsed().as_secs_f64());
                    self.print_tool_result(&name, &detail, note.as_deref(), failed, elapsed);
                }
            }
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
            Event::Activity(activity) => {
                if !matches!(activity, Activity::Connected) {
                    self.record_model_activity();
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
                            Activity::Thinking => "Thinking · ctrl+c cancel".to_owned(),
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
            // Reasoning stays out of the terminal; the spinner already says the
            // model is thinking, and the transcript keeps it for remote viewers.
            Event::Reasoning(_) => self.record_model_activity(),
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
        if self.pending.is_empty() {
            return;
        }
        let text = self.pending.take();
        let width = (console::Term::stdout().size().1 as usize)
            .saturating_sub(4)
            .clamp(12, 96);
        let text = self.reflow.push(&text, width, "    ");
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

fn token_count(tokens: usize) -> String {
    if tokens < 1000 {
        format!("{tokens} tokens")
    } else {
        let value = tokens as f64 / 1000.0;
        format!("{value:.1}k tokens")
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
        "edit_file" => "Edit file".into(),
        "write_file" => "Write file".into(),
        "list_files" => "List files".into(),
        "search" => "Search".into(),
        "shell" => "Run command".into(),
        _ => safe(name).replace('_', " "),
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.clear_spinner();
    }
}
pub fn approve(action: &Action) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    eprintln!(
        "\n  {}\n{}",
        style("◇ approval required").yellow().bold(),
        safe(&action.description())
    );
    eprint!("  Allow this action? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
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

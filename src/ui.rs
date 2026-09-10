use crate::agent::AgentEvent;
use builder_provider::{Activity, Event};
use builder_tools::Action;
use console::style;
use std::io::{self, IsTerminal, Write};

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
    line_start: bool,
    prompt_tokens: usize,
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
            line_start: true,
            prompt_tokens: 0,
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
                self.flush();
                self.clear_spinner();
                let percent = threshold * 100 / context_tokens.max(1);
                eprintln!(
                    "\n  Auto-compact triggered: {} conversation + {overhead} tool schemas + {reserved} reserved for output = {} estimated tokens, at or past the {threshold} trigger ({percent}% of {context_tokens})",
                    messages,
                    messages + overhead + reserved
                );
            }
            AgentEvent::Compacting {
                before,
                context_tokens,
            } => {
                self.flush();
                self.clear_spinner();
                self.compacting = true;
                eprintln!(
                    "\n  Compacting context ({before} estimated tokens / {context_tokens} configured) · originals retained · ctrl+c cancel"
                );
            }
            AgentEvent::Compacted {
                before,
                after,
                context_tokens,
            } => {
                self.clear_spinner();
                self.compacting = false;
                eprintln!(
                    "\n  Context compacted: {before} → {after} estimated tokens / {context_tokens} configured · /history archived keeps originals"
                );
            }
            AgentEvent::SummaryRecovery { size, limit } => {
                self.flush();
                self.clear_spinner();
                eprintln!(
                    "\n  Compaction handoff too long ({size} estimated tokens; limit {limit}) · retrying once with a shorter handoff"
                );
            }
            AgentEvent::MemoryNotice(note) => {
                self.flush();
                self.clear_spinner();
                eprintln!("\n  {}", safe(&note));
            }
            AgentEvent::ExplorationRecovery { calls } => {
                self.flush();
                self.clear_spinner();
                eprintln!(
                    "\n  Progress check · {calls} tool calls without file progress · focusing the next action · full context preserved"
                );
            }
            AgentEvent::OutputRecovery { budget } => {
                self.flush();
                self.clear_spinner();
                let operation = if self.compacting {
                    "Compaction"
                } else {
                    "Model"
                };
                eprintln!(
                    "\n  {operation} output limit reached · partial output discarded · retrying once with {budget} output tokens"
                );
            }
            AgentEvent::RepetitionNotice { name, count, limit } => {
                self.flush();
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
                self.flush();
                self.clear_spinner();
                if self.interactive {
                    if self.started {
                        println!();
                        self.started = false;
                        self.line_start = true;
                    }
                    self.tool_started_at = Some(std::time::Instant::now());
                    let label = tool_label(&name);
                    self.start_spinner(match self.fit(&detail, label.len() + 22) {
                        detail if detail.is_empty() => format!("{label} · ctrl+c cancel"),
                        detail => format!("{label}  {detail} · ctrl+c cancel"),
                    });
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
                    let label = tool_label(&name);
                    // What the call targeted outranks the note: a short note
                    // stays on the line, a long reason moves below it rather
                    // than crushing the path or command out of the display.
                    let inline = note
                        .as_deref()
                        .filter(|note| note.chars().count() <= 24)
                        .map_or_else(String::new, |note| format!("  {note}"));
                    eprintln!(
                        "  {} {}  {}{}  {}",
                        if failed {
                            theme::warning("!")
                        } else {
                            theme::success("✓")
                        },
                        label,
                        self.fit(&detail, label.len() + inline.chars().count() + 22),
                        theme::muted(&inline),
                        theme::muted(&format!("{elapsed:.1}s"))
                    );
                    if let Some(note) = note.filter(|_| inline.is_empty()) {
                        eprintln!("      {}", theme::muted(&self.fit(&note, 8)));
                    }
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
        crate::input::layout::clip(&safe(text), width.clamp(12, 120))
    }
    fn clear_spinner(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.finish_and_clear();
        }
    }
    fn start_spinner(&mut self, message: String) {
        let spinner = indicatif::ProgressBar::new_spinner();
        spinner.set_style(
            indicatif::ProgressStyle::with_template("  {spinner:.cyan} {msg}  {elapsed:.dim}")
                .expect("static progress template")
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        spinner.set_message(self.fit(&message, 14));
        spinner.enable_steady_tick(std::time::Duration::from_millis(120));
        self.spinner = Some(spinner);
    }
    fn model_event(&mut self, event: Event) {
        match event {
            // The estimator the agent budgets with, so the waiting states and
            // the compaction notice quote the same number.
            Event::Prompt { bytes } => self.prompt_tokens = bytes.div_ceil(2),
            Event::Activity(activity) => {
                let size = self.prompt_size();
                if let Some(spinner) = &self.spinner {
                    let message = if self.compacting {
                        "Summarizing context · originals retained · ctrl+c cancel".to_owned()
                    } else {
                        match activity {
                            Activity::Connected => format!("Reading {size} · ctrl+c cancel"),
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
                self.flush();
                self.started = false;
                self.line_start = true;
                self.pending.reset();
                if self.interactive {
                    self.clear_spinner();
                    if !self.heading_printed || number > 1 {
                        eprintln!(
                            "\n  {}",
                            theme::accent(&if number == 1 {
                                "builder".to_owned()
                            } else {
                                format!("builder · attempt {number}/{maximum}")
                            })
                        );
                        self.heading_printed = true;
                    }
                    let size = self.prompt_size();
                    self.start_spinner(format!("Sending {size} · ctrl+c cancel"));
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
            Event::Reasoning(_) => {}
            Event::Retry { delay_ms, reason } => {
                self.flush();
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
        let mut out = io::stdout().lock();
        for part in text.split_inclusive('\n') {
            if self.line_start && part != "\n" {
                let _ = out.write_all(b"  ");
            }
            let _ = out.write_all(part.as_bytes());
            self.line_start = part.ends_with('\n');
        }
        let _ = out.flush();
        self.started = true;
    }
    pub fn finish(&mut self) {
        self.flush();
        self.clear_spinner();
        if self.started {
            println!();
        }
        if self.interactive && self.bytes > 0 {
            eprintln!(
                "\n  {}\n",
                theme::muted(&format!(
                    "{:.1}s{}",
                    self.started_at.elapsed().as_secs_f64(),
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

//! Interactive pipeline settings. Storage and live agent updates stay in the app.
use crate::ui::{safe, theme};
use builder_core::config::PipelineSettings;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, BufRead, Write};

const FIELDS: &[(&str, &str, &str)] = &[
    (
        "enabled",
        "Research pipeline",
        "Master switch for the added research workflow.",
    ),
    (
        "guidance",
        "Workflow suggestions",
        "Suggest research steps automatically.",
    ),
    (
        "planning",
        "Acceptance plans",
        "Record criteria before implementing a task.",
    ),
    (
        "observations",
        "Contract observations",
        "Inspect versioned files, JSON values and source anchors.",
    ),
    (
        "symbols",
        "Source navigation",
        "Find declarations and references in source code.",
    ),
    (
        "semantic",
        "Language servers",
        "Allow approved language-server navigation and diagnostics.",
    ),
    (
        "hypotheses",
        "Hypotheses",
        "Record explanations and tests that could disprove them.",
    ),
    (
        "verification",
        "Verification",
        "Run approved checks with source freshness tracking.",
    ),
    (
        "candidates",
        "Candidate fixes",
        "Test and apply alternatives; commands still require approval.",
    ),
    (
        "review",
        "Independent review",
        "Require a model review of check coverage for verified finishes.",
    ),
    (
        "completion_gate",
        "Completion checks",
        "Require a recorded outcome before accepting planned-task completion.",
    ),
    (
        "procedures",
        "Learned procedures",
        "Learn and recall procedures. Learning/recall also need memory enabled.",
    ),
    (
        "auto_recall",
        "Automatic recall",
        "Include relevant procedures automatically when memory is enabled.",
    ),
    (
        "history",
        "History retrieval",
        "Search and page archived conversation evidence.",
    ),
    (
        "analysis",
        "Source analysis",
        "Run bounded independent source analyses and synthesis.",
    ),
    (
        "phase_routing",
        "Phase-based tool schemas",
        "Shrink research operations from typed durable progress; does not classify prompt wording or model names.",
    ),
    (
        "code_index",
        "Code index",
        "Enable checkout-scoped structural, lexical, graph and semantic code retrieval.",
    ),
    (
        "code_index_background",
        "Background indexing",
        "Refresh the complete code index while the interactive composer is idle.",
    ),
    (
        "code_index_watch",
        "Watch code changes",
        "Use recursive filesystem events for prompt refreshes; periodic scans remain the correctness fallback.",
    ),
    (
        "code_index_semantic",
        "Semantic code retrieval",
        "Embed code chunks locally when embeddings are enabled; lexical and structural retrieval remain available on failure.",
    ),
    (
        "code_index_auto_context",
        "Automatic code context",
        "Include a small current, source-validated lexical code packet with a new request when the index has support.",
    ),
    (
        "code_index_telemetry",
        "Retrieval telemetry",
        "Store bounded query/rank metadata for evaluation; source excerpts and model output are not copied.",
    ),
    (
        "code_history",
        "Repository history",
        "Index recent local Git commits and changed paths as historical ranking evidence.",
    ),
    (
        "todos",
        "Todo board",
        "Let the agent keep a visible, ordered todo list and follow it step by step.",
    ),
    (
        "subagents",
        "Subagents",
        "Let the agent delegate read-only investigations to subagents with their own context.",
    ),
    (
        "code_index_max_files",
        "Index files",
        "1–50000 source files per complete index generation.",
    ),
    (
        "code_index_max_file_bytes",
        "Index file size (bytes)",
        "1024–2097152 bytes per source file; oversized files are excluded and counted.",
    ),
    (
        "code_index_max_bytes",
        "Index total size (bytes)",
        "1024–268435456 source bytes. Exceeding this keeps the previous complete generation.",
    ),
    (
        "code_index_max_chunks",
        "Index chunks",
        "1–100000 structural chunks. Exceeding this keeps the previous complete generation.",
    ),
    (
        "code_index_refresh_secs",
        "Index refresh (seconds)",
        "1–3600 seconds between bounded fallback scans, including when filesystem events are unavailable.",
    ),
    (
        "code_index_debounce_ms",
        "Index debounce (milliseconds)",
        "50–10000 milliseconds to combine bursts of filesystem events before rebuilding.",
    ),
    (
        "code_index_embedding_timeout_secs",
        "Code embedding timeout (seconds)",
        "1–600 seconds per local or remote code-embedding batch.",
    ),
    (
        "code_index_embedding_batch",
        "Code embedding batch size",
        "1–128 missing content-addressed vectors generated together while idle.",
    ),
    (
        "code_search_results",
        "Code search results",
        "1–20 fresh, diversified chunks returned to the model.",
    ),
    (
        "code_index_lexical_candidates",
        "Lexical candidate pool",
        "1–500 FTS5/BM25 candidates considered before fusion.",
    ),
    (
        "code_index_exact_candidates",
        "Exact candidate pool",
        "1–500 exact symbol and reference candidates considered before fusion.",
    ),
    (
        "code_index_dense_candidates",
        "Semantic candidate pool",
        "1–500 embedding candidates considered after the semantic score floor.",
    ),
    (
        "code_index_history_results",
        "History candidate pool",
        "1–100 Git history matches considered as path-ranking evidence.",
    ),
    (
        "code_index_chunks_per_file",
        "Chunks per result file",
        "1–8 chunks from one file allowed in a diversified code-search result.",
    ),
    (
        "code_index_auto_candidates",
        "Automatic candidate pool",
        "1–200 lexical candidates considered for automatic current-code context.",
    ),
    (
        "code_index_auto_files",
        "Automatic context files",
        "1–12 source-validated files included in automatic current-code context.",
    ),
    (
        "code_index_min_similarity_percent",
        "Minimum semantic score (%)",
        "0–100 minimum cosine similarity for semantic-only candidates; lexical matches are unaffected.",
    ),
    (
        "code_index_graph_hops",
        "Symbol graph hops",
        "0–3 bounded exact declaration/reference expansion hops per code search.",
    ),
    (
        "code_index_graph_symbols",
        "Graph symbols per hop",
        "1–32 unique frontier symbols expanded at each graph hop.",
    ),
    (
        "code_index_graph_candidates",
        "Graph candidates per hop",
        "1–200 exact symbol/reference candidates considered at each hop.",
    ),
    (
        "code_history_commits",
        "History commits",
        "1–5000 recent commits retained for checkout-local history retrieval.",
    ),
    (
        "code_history_timeout_secs",
        "History timeout (seconds)",
        "1–30 seconds for bounded local Git history capture.",
    ),
    (
        "memory_min_similarity_percent",
        "Memory semantic score (%)",
        "0–100 minimum cosine similarity before dense memory retrieval is trusted.",
    ),
    (
        "memory_min_margin_percent",
        "Memory semantic margin (%)",
        "0–100 minimum top-versus-runner-up similarity margin; lexical matches remain available.",
    ),
    (
        "candidate_attempts",
        "Candidate attempts",
        "1–20 claimed candidate attempts per user turn.",
    ),
    (
        "completion_retries",
        "Completion retries",
        "0–8 corrective attempts per run.",
    ),
    (
        "analysis_artifacts",
        "Analysis excerpts",
        "1–4 source excerpts per analysis.",
    ),
    (
        "analysis_timeout_secs",
        "Analysis timeout (seconds)",
        "1–120 seconds per analysis or review call.",
    ),
    (
        "analysis_output_tokens",
        "Analysis output (tokens)",
        "256–8192 output tokens per analysis or review call.",
    ),
    (
        "command_timeout_secs",
        "Check timeout (seconds)",
        "1–120 seconds maximum per research command.",
    ),
    (
        "semantic_timeout_secs",
        "Language-server timeout (seconds)",
        "1–120 seconds per server session.",
    ),
    (
        "diagnostic_wait_secs",
        "Diagnostic wait (seconds)",
        "1–120 seconds, within the server lifetime.",
    ),
    (
        "snapshot_max_files",
        "Snapshot files",
        "1–20000 source files. Exceeding this fails the snapshot.",
    ),
    (
        "snapshot_max_file_bytes",
        "Snapshot file size (bytes)",
        "1–2097152 bytes per file. Partial evidence is never trusted.",
    ),
    (
        "snapshot_max_bytes",
        "Snapshot total size (bytes)",
        "1–67108864 source bytes total.",
    ),
    (
        "history_page_bytes",
        "History page size (bytes)",
        "256–8000 bytes per original-history page.",
    ),
    (
        "history_search_results",
        "History search results",
        "1–16 results per page.",
    ),
    (
        "procedure_results",
        "Procedure results",
        "1–4 procedures per retrieval.",
    ),
    (
        "retrieval_records",
        "Evidence window",
        "1–256 recent records. Missing proof requires revalidation.",
    ),
    (
        "max_rounds",
        "Agent rounds per run",
        "1–1000 model rounds per run or /retry; default 100. Loop guards remain active. Saving replaces any command-line round override in this session.",
    ),
    (
        "progress_check_calls",
        "Progress check after calls",
        "1–1000 completed tools without a file change before the progress nudge. A lighter planning reminder starts at half this.",
    ),
    (
        "progress_recovery_rounds",
        "Progress recovery rounds",
        "1–1000 guided model rounds before a tool-free conclusion. The call ceiling is this plus Progress check after calls.",
    ),
    (
        "failure_check_calls",
        "Failure check after calls",
        "1–100 consecutive failed tools before recovery guidance.",
    ),
    (
        "failure_recovery_rounds",
        "Failure recovery rounds",
        "1–100 guided model rounds allowed for repeated tool failures.",
    ),
    (
        "identical_shell_calls",
        "Identical shell calls",
        "1–100 completed identical shell calls allowed before rejecting another.",
    ),
    (
        "tool_calls_per_response",
        "Tool calls per response",
        "1–128 tool calls allowed in one model response.",
    ),
    (
        "parallel_tools",
        "Parallel read-only tools",
        "1–32 consecutive reads, searches and subagents run at once; 1 runs every tool in order.",
    ),
    (
        "subagent_parallel",
        "Concurrent subagents",
        "1–8 subagents talking to the model at once. Match your server's parallel request slots.",
    ),
    (
        "subagent_rounds",
        "Subagent rounds",
        "1–200 model rounds per subagent before it must report what it found.",
    ),
];
const SAVE: usize = FIELDS.len();
const RESET: usize = SAVE + 1;
const CANCEL: usize = SAVE + 2;
struct Menu {
    draft: PipelineSettings,
    selected: usize,
    editing: Option<String>,
    note: String,
}
enum Outcome {
    Continue,
    Save,
    Cancel,
}
impl Menu {
    fn new(settings: &PipelineSettings) -> Self {
        Self {
            draft: settings.clone(),
            selected: 0,
            editing: None,
            note: String::new(),
        }
    }
    fn value(&self, index: usize) -> String {
        let value = serde_json::to_value(&self.draft).expect("typed settings serialize");
        let value = &value[FIELDS[index].0];
        if let Some(on) = value.as_bool() {
            if on { "On" } else { "Off" }.into()
        } else {
            value.to_string()
        }
    }
    fn activate(&mut self) -> Outcome {
        self.note.clear();
        match self.selected {
            SAVE => return Outcome::Save,
            RESET => {
                self.draft = PipelineSettings::default();
                self.note = "Defaults restored in this menu; Save to apply.".into();
            }
            CANCEL => return Outcome::Cancel,
            index => {
                let value = self.value(index);
                if value == "On" || value == "Off" {
                    self.change(if value == "On" { "false" } else { "true" });
                } else {
                    self.editing = Some(String::new());
                    self.note = format!(
                        "Current value: {value}. Type a number, Enter to accept, Esc to keep it."
                    );
                }
            }
        }
        Outcome::Continue
    }
    fn change(&mut self, value: &str) {
        match self
            .draft
            .updated(&[format!("{}={value}", FIELDS[self.selected].0)])
        {
            Ok(settings) => {
                self.draft = settings;
                self.editing = None;
                self.note.clear();
            }
            Err(error) => self.note = error.to_string(),
        }
    }
    fn key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Outcome::Cancel;
        }
        if self.editing.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.editing = None;
                    self.note.clear();
                }
                KeyCode::Enter => {
                    let value = self.editing.clone().unwrap();
                    if value.is_empty() {
                        self.editing = None;
                        self.note.clear();
                    } else {
                        self.change(&value);
                    }
                }
                KeyCode::Backspace => {
                    self.editing.as_mut().unwrap().pop();
                }
                KeyCode::Char(c)
                    if c.is_ascii_digit() && self.editing.as_ref().unwrap().len() < 20 =>
                {
                    self.editing.as_mut().unwrap().push(c)
                }
                _ => {}
            }
            return Outcome::Continue;
        }
        match key.code {
            KeyCode::Esc => Outcome::Cancel,
            KeyCode::Up => {
                self.selected = (self.selected + CANCEL) % (CANCEL + 1);
                Outcome::Continue
            }
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1) % (CANCEL + 1);
                Outcome::Continue
            }
            KeyCode::Home => {
                self.selected = 0;
                Outcome::Continue
            }
            KeyCode::End => {
                self.selected = SAVE;
                Outcome::Continue
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            _ => Outcome::Continue,
        }
    }
}
struct Terminal;
impl Terminal {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(guard)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// The save callback persists before this returns. Failure leaves the editable
/// draft in the menu; Escape never calls the callback.
pub fn edit(
    settings: &PipelineSettings,
    profile: &str,
    plain: bool,
    mut save: impl FnMut(&PipelineSettings) -> Result<(), String>,
) -> io::Result<Option<PipelineSettings>> {
    let mut menu = Menu::new(settings);
    if plain {
        return plain_menu(&mut menu, profile, &mut save);
    }
    let _terminal = Terminal::enter()?;
    loop {
        draw(&menu, profile)?;
        if let Event::Key(key) = event::read()? {
            match menu.key(key) {
                Outcome::Continue => {}
                Outcome::Cancel => return Ok(None),
                Outcome::Save => match save(&menu.draft) {
                    Ok(()) => return Ok(Some(menu.draft)),
                    Err(error) => menu.note = error,
                },
            }
        }
    }
}
fn draw(menu: &Menu, profile: &str) -> io::Result<()> {
    let (width, height) = terminal::size()?;
    let width = width.clamp(1, 100).saturating_sub(4) as usize;
    let mut out = io::stdout().lock();
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    let title = format!("Pipeline settings · {}", safe(profile));
    let visible = height.saturating_sub(7).max(1) as usize;
    let start = menu.selected.saturating_sub(visible.saturating_sub(1));
    let mut lines = vec![
        title,
        "↑↓ choose · Enter/Space change · End jumps to Save · Esc cancels".into(),
        "Save applies to this session and its profile. Nothing changes until saved.".into(),
        "─".repeat(width),
    ];
    for index in (0..=CANCEL).skip(start).take(visible) {
        let selected = if index == menu.selected { "›" } else { " " };
        let label = match index {
            SAVE => "Save settings".into(),
            RESET => "Restore defaults".into(),
            CANCEL => "Cancel".into(),
            _ => format!(
                "{}: {}{}",
                FIELDS[index].1,
                menu.value(index),
                if index == menu.selected {
                    menu.editing
                        .as_ref()
                        .map(|s| format!(" → {s}_"))
                        .unwrap_or_default()
                } else {
                    String::new()
                }
            ),
        };
        lines.push(format!("{selected} {label}"));
    }
    lines.push(String::new());
    lines.push(if menu.note.is_empty() {
        if menu.selected < FIELDS.len() {
            FIELDS[menu.selected].2.into()
        } else {
            "Save commits all shown values; Cancel discards the draft.".into()
        }
    } else {
        safe(&menu.note)
    });
    for (row, line) in lines.iter().take(height as usize).enumerate() {
        queue!(out, cursor::MoveTo(0, row as u16))?;
        let line = super::layout::clip(line, width);
        let painted = if row == 0 {
            theme::accent(&line)
        } else if line.starts_with('›') {
            theme::selected(&format!(
                "{line}{}",
                " ".repeat(
                    width.saturating_sub(unicode_width::UnicodeWidthStr::width(line.as_str()))
                )
            ))
        } else if row == 3 {
            theme::border(&line)
        } else if row < 4 || row >= 4 + visible {
            theme::muted(&line)
        } else {
            line
        };
        write!(out, "  {painted}")?;
    }
    out.flush()
}
fn read_choice() -> io::Result<Option<String>> {
    read_choice_from(&mut io::stdin().lock())
}
fn read_choice_from(input: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let buffer = input.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let end = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(buffer.len());
        let keep = end.min(128usize.saturating_sub(line.len()));
        line.extend_from_slice(&buffer[..keep]);
        overflow |= keep < end;
        let done = buffer[end - 1] == b'\n';
        input.consume(end);
        if done {
            break;
        }
    }
    // Consume the whole rejected line so it cannot leak into the chat composer.
    if overflow {
        return Err(io::Error::other("Menu input exceeds 128 bytes"));
    }
    if line.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8(line)
            .map_err(io::Error::other)?
            .trim()
            .to_owned(),
    ))
}

fn plain_menu(
    menu: &mut Menu,
    profile: &str,
    save: &mut impl FnMut(&PipelineSettings) -> Result<(), String>,
) -> io::Result<Option<PipelineSettings>> {
    loop {
        println!(
            "\nPipeline settings · {}\nSave applies to this session and its profile.",
            safe(profile)
        );
        for (i, (_, label, _)) in FIELDS.iter().enumerate() {
            println!("{:>2}. {label}: {}", i + 1, menu.value(i));
        }
        println!("s. Save settings   r. Restore defaults   q. Cancel");
        if !menu.note.is_empty() {
            println!("{}", safe(&menu.note));
        }
        print!("Choose an item: ");
        io::stdout().flush()?;
        let Some(choice) = read_choice()? else {
            return Ok(None);
        };
        match choice.as_str() {
            "q" | "" => return Ok(None),
            "s" => match save(&menu.draft) {
                Ok(()) => return Ok(Some(menu.draft.clone())),
                Err(e) => menu.note = e,
            },
            "r" => {
                menu.draft = PipelineSettings::default();
                menu.note = "Defaults restored; Save to apply.".into();
            }
            _ => {
                if let Ok(index) = choice.parse::<usize>() {
                    if (1..=FIELDS.len()).contains(&index) {
                        menu.selected = index - 1;
                        menu.activate();
                        if menu.editing.is_some() {
                            println!("{}", FIELDS[index - 1].2);
                            print!("New value (Enter keeps current): ");
                            io::stdout().flush()?;
                            let Some(value) = read_choice()? else {
                                return Ok(None);
                            };
                            if !value.is_empty() {
                                menu.change(&value);
                            }
                            menu.editing = None;
                        }
                    } else {
                        menu.note = "Choose a listed item.".into();
                    }
                } else {
                    menu.note = "Choose an item number, s, r or q.".into();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn oversized_plain_choice_is_drained_before_returning_to_chat() {
        let data = format!("{}\nnext prompt\n", "9".repeat(200));
        let mut input = io::Cursor::new(data);
        assert!(read_choice_from(&mut input).is_err());
        assert_eq!(
            read_choice_from(&mut input).unwrap().as_deref(),
            Some("next prompt")
        );
    }
    #[test]
    fn menu_covers_every_setting_and_cancel_does_not_change_original() {
        let original = PipelineSettings::default();
        let fields = serde_json::to_value(&original).unwrap();
        assert_eq!(FIELDS.len(), fields.as_object().unwrap().len());
        for (key, _, _) in FIELDS {
            assert!(fields.get(*key).is_some());
        }
        let mut menu = Menu::new(&original);
        menu.key(key(KeyCode::Enter));
        assert!(!menu.draft.enabled);
        assert!(matches!(menu.key(key(KeyCode::Esc)), Outcome::Cancel));
        assert!(original.enabled);
    }
    #[test]
    fn number_editor_validates_and_navigation_reaches_save() {
        let mut menu = Menu::new(&PipelineSettings::default());
        menu.selected = FIELDS
            .iter()
            .position(|(key, _, _)| *key == "candidate_attempts")
            .unwrap();
        menu.activate();
        menu.key(key(KeyCode::Char('0')));
        menu.key(key(KeyCode::Enter));
        assert!(menu.editing.is_some());
        assert_eq!(menu.draft.candidate_attempts, 3);
        menu.key(key(KeyCode::Backspace));
        menu.key(key(KeyCode::Char('5')));
        menu.key(key(KeyCode::Enter));
        assert_eq!(menu.draft.candidate_attempts, 5);
        assert!(menu.editing.is_none());
        menu.key(key(KeyCode::End));
        assert!(matches!(menu.key(key(KeyCode::Enter)), Outcome::Save));
        menu.selected = RESET;
        menu.activate();
        assert_eq!(menu.draft, PipelineSettings::default());
    }
}

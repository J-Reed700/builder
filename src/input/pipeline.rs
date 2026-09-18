//! Interactive pipeline settings. Storage and live agent updates stay in the app.
use super::menu::{self, Screen, Value, View};
use crate::ui::safe;
use builder_core::config::PipelineSettings;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::io::{self, Write};

/// Setting key, menu label, footer description, and section heading. Entries are
/// grouped by section in display order; the menu inserts a heading on each change.
type Field = (&'static str, &'static str, &'static str, &'static str);

const FIELDS: &[Field] = &[
    (
        "enabled",
        "Research pipeline",
        "Master switch for the added research workflow.",
        "Research workflow",
    ),
    (
        "guidance",
        "Workflow suggestions",
        "Suggest research steps automatically.",
        "Research workflow",
    ),
    (
        "planning",
        "Acceptance plans",
        "Record criteria before implementing a task.",
        "Research workflow",
    ),
    (
        "hypotheses",
        "Hypotheses",
        "Record explanations and tests that could disprove them.",
        "Research workflow",
    ),
    (
        "verification",
        "Verification",
        "Run approved checks with source freshness tracking.",
        "Research workflow",
    ),
    (
        "candidates",
        "Candidate fixes",
        "Test and apply alternatives; commands still require approval.",
        "Research workflow",
    ),
    (
        "review",
        "Independent review",
        "Require a model review of check coverage for verified finishes.",
        "Research workflow",
    ),
    (
        "completion_gate",
        "Completion checks",
        "Require a recorded outcome before accepting planned-task completion.",
        "Research workflow",
    ),
    (
        "analysis",
        "Source analysis",
        "Run bounded independent source analyses and synthesis.",
        "Research workflow",
    ),
    (
        "phase_routing",
        "Phase-based tool schemas",
        "Shrink research operations from typed durable progress; does not classify prompt wording or model names.",
        "Research workflow",
    ),
    (
        "code_index",
        "Code index",
        "Enable checkout-scoped structural, lexical, graph and semantic code retrieval.",
        "Code intelligence",
    ),
    (
        "code_index_background",
        "Background indexing",
        "Refresh the complete code index while the interactive composer is idle.",
        "Code intelligence",
    ),
    (
        "code_index_watch",
        "Watch code changes",
        "Use recursive filesystem events for prompt refreshes; periodic scans remain the correctness fallback.",
        "Code intelligence",
    ),
    (
        "code_index_semantic",
        "Semantic code retrieval",
        "Embed code chunks locally when embeddings are enabled; lexical and structural retrieval remain available on failure.",
        "Code intelligence",
    ),
    (
        "code_index_auto_context",
        "Automatic code context",
        "Include a small current, source-validated lexical code packet with a new request when the index has support.",
        "Code intelligence",
    ),
    (
        "code_index_telemetry",
        "Retrieval telemetry",
        "Store bounded query/rank metadata for evaluation; source excerpts and model output are not copied.",
        "Code intelligence",
    ),
    (
        "code_history",
        "Repository history",
        "Index recent local Git commits and changed paths as historical ranking evidence.",
        "Code intelligence",
    ),
    (
        "observations",
        "Contract observations",
        "Inspect versioned files, JSON values and source anchors.",
        "Code intelligence",
    ),
    (
        "symbols",
        "Source navigation",
        "Find declarations and references in source code.",
        "Code intelligence",
    ),
    (
        "semantic",
        "Language servers",
        "Allow approved language-server navigation and diagnostics.",
        "Code intelligence",
    ),
    (
        "procedures",
        "Learned procedures",
        "Learn and recall procedures. Learning/recall also need memory enabled.",
        "Memory and history",
    ),
    (
        "auto_recall",
        "Automatic recall",
        "Include relevant procedures automatically when memory is enabled.",
        "Memory and history",
    ),
    (
        "history",
        "History retrieval",
        "Search and page archived conversation evidence.",
        "Memory and history",
    ),
    (
        "todos",
        "Todo board",
        "Let the agent keep a visible, ordered todo list and follow it step by step.",
        "Agent",
    ),
    (
        "subagents",
        "Subagents",
        "Let the agent delegate read-only investigations to subagents with their own context.",
        "Agent",
    ),
    (
        "max_rounds",
        "Agent rounds per run",
        "1–1000 model rounds per run or /retry; default 100. Loop guards remain active. Saving replaces any command-line round override in this session.",
        "Agent",
    ),
    (
        "parallel_tools",
        "Parallel read-only tools",
        "1–32 consecutive reads, searches and subagents run at once; 1 runs every tool in order.",
        "Agent",
    ),
    (
        "subagent_parallel",
        "Concurrent subagents",
        "1–8 subagents talking to the model at once. Match your server's parallel request slots.",
        "Agent",
    ),
    (
        "subagent_rounds",
        "Subagent rounds",
        "1–200 model rounds per subagent before it must report what it found.",
        "Agent",
    ),
    (
        "progress_check_calls",
        "Progress check after calls",
        "1–1000 completed tools without a file change before the progress nudge. A lighter planning reminder starts at half this.",
        "Loop guards",
    ),
    (
        "progress_recovery_rounds",
        "Progress recovery rounds",
        "1–1000 guided model rounds before a tool-free conclusion. The call ceiling is this plus Progress check after calls.",
        "Loop guards",
    ),
    (
        "failure_check_calls",
        "Failure check after calls",
        "1–100 consecutive failed tools before recovery guidance.",
        "Loop guards",
    ),
    (
        "failure_recovery_rounds",
        "Failure recovery rounds",
        "1–100 guided model rounds allowed for repeated tool failures.",
        "Loop guards",
    ),
    (
        "identical_shell_calls",
        "Identical shell calls",
        "1–100 completed identical shell calls allowed before rejecting another.",
        "Loop guards",
    ),
    (
        "tool_calls_per_response",
        "Tool calls per response",
        "1–128 tool calls allowed in one model response.",
        "Loop guards",
    ),
    (
        "candidate_attempts",
        "Candidate attempts",
        "1–20 claimed candidate attempts per user turn.",
        "Research budgets",
    ),
    (
        "completion_retries",
        "Completion retries",
        "0–8 corrective attempts per run.",
        "Research budgets",
    ),
    (
        "analysis_artifacts",
        "Analysis excerpts",
        "1–4 source excerpts per analysis.",
        "Research budgets",
    ),
    (
        "analysis_timeout_secs",
        "Analysis timeout (seconds)",
        "1–120 seconds per analysis or review call.",
        "Research budgets",
    ),
    (
        "analysis_output_tokens",
        "Analysis output (tokens)",
        "256–8192 output tokens per analysis or review call.",
        "Research budgets",
    ),
    (
        "command_timeout_secs",
        "Check timeout (seconds)",
        "1–120 seconds maximum per research command.",
        "Research budgets",
    ),
    (
        "semantic_timeout_secs",
        "Language-server timeout (seconds)",
        "1–120 seconds per server session.",
        "Research budgets",
    ),
    (
        "diagnostic_wait_secs",
        "Diagnostic wait (seconds)",
        "1–120 seconds, within the server lifetime.",
        "Research budgets",
    ),
    (
        "snapshot_max_files",
        "Snapshot files",
        "1–20000 source files. Exceeding this fails the snapshot.",
        "Research budgets",
    ),
    (
        "snapshot_max_file_bytes",
        "Snapshot file size (bytes)",
        "1–2097152 bytes per file. Partial evidence is never trusted.",
        "Research budgets",
    ),
    (
        "snapshot_max_bytes",
        "Snapshot total size (bytes)",
        "1–67108864 source bytes total.",
        "Research budgets",
    ),
    (
        "code_index_max_files",
        "Index files",
        "1–50000 source files per complete index generation.",
        "Code index budgets",
    ),
    (
        "code_index_max_file_bytes",
        "Index file size (bytes)",
        "1024–2097152 bytes per source file; oversized files are excluded and counted.",
        "Code index budgets",
    ),
    (
        "code_index_max_bytes",
        "Index total size (bytes)",
        "1024–268435456 source bytes. Exceeding this keeps the previous complete generation.",
        "Code index budgets",
    ),
    (
        "code_index_max_chunks",
        "Index chunks",
        "1–100000 structural chunks. Exceeding this keeps the previous complete generation.",
        "Code index budgets",
    ),
    (
        "code_index_refresh_secs",
        "Index refresh (seconds)",
        "1–3600 seconds between bounded fallback scans, including when filesystem events are unavailable.",
        "Code index budgets",
    ),
    (
        "code_index_debounce_ms",
        "Index debounce (milliseconds)",
        "50–10000 milliseconds to combine bursts of filesystem events before rebuilding.",
        "Code index budgets",
    ),
    (
        "code_index_embedding_timeout_secs",
        "Code embedding timeout (seconds)",
        "1–600 seconds per local or remote code-embedding batch.",
        "Code index budgets",
    ),
    (
        "code_index_embedding_batch",
        "Code embedding batch size",
        "1–128 missing content-addressed vectors generated together while idle.",
        "Code index budgets",
    ),
    (
        "code_search_results",
        "Code search results",
        "1–20 fresh, diversified chunks returned to the model.",
        "Code retrieval",
    ),
    (
        "code_index_lexical_candidates",
        "Lexical candidate pool",
        "1–500 FTS5/BM25 candidates considered before fusion.",
        "Code retrieval",
    ),
    (
        "code_index_exact_candidates",
        "Exact candidate pool",
        "1–500 exact symbol and reference candidates considered before fusion.",
        "Code retrieval",
    ),
    (
        "code_index_dense_candidates",
        "Semantic candidate pool",
        "1–500 embedding candidates considered after the semantic score floor.",
        "Code retrieval",
    ),
    (
        "code_index_history_results",
        "History candidate pool",
        "1–100 Git history matches considered as path-ranking evidence.",
        "Code retrieval",
    ),
    (
        "code_index_chunks_per_file",
        "Chunks per result file",
        "1–8 chunks from one file allowed in a diversified code-search result.",
        "Code retrieval",
    ),
    (
        "code_index_auto_candidates",
        "Automatic candidate pool",
        "1–200 lexical candidates considered for automatic current-code context.",
        "Code retrieval",
    ),
    (
        "code_index_auto_files",
        "Automatic context files",
        "1–12 source-validated files included in automatic current-code context.",
        "Code retrieval",
    ),
    (
        "code_index_min_similarity_percent",
        "Minimum semantic score (%)",
        "0–100 minimum cosine similarity for semantic-only candidates; lexical matches are unaffected.",
        "Code retrieval",
    ),
    (
        "code_index_graph_hops",
        "Symbol graph hops",
        "0–3 bounded exact declaration/reference expansion hops per code search.",
        "Code retrieval",
    ),
    (
        "code_index_graph_symbols",
        "Graph symbols per hop",
        "1–32 unique frontier symbols expanded at each graph hop.",
        "Code retrieval",
    ),
    (
        "code_index_graph_candidates",
        "Graph candidates per hop",
        "1–200 exact symbol/reference candidates considered at each hop.",
        "Code retrieval",
    ),
    (
        "code_history_commits",
        "History commits",
        "1–5000 recent commits retained for checkout-local history retrieval.",
        "Code retrieval",
    ),
    (
        "code_history_timeout_secs",
        "History timeout (seconds)",
        "1–30 seconds for bounded local Git history capture.",
        "Code retrieval",
    ),
    (
        "memory_min_similarity_percent",
        "Memory semantic score (%)",
        "0–100 minimum cosine similarity before dense memory retrieval is trusted.",
        "Memory retrieval",
    ),
    (
        "memory_min_margin_percent",
        "Memory semantic margin (%)",
        "0–100 minimum top-versus-runner-up similarity margin; lexical matches remain available.",
        "Memory retrieval",
    ),
    (
        "procedure_results",
        "Procedure results",
        "1–4 procedures per retrieval.",
        "Memory retrieval",
    ),
    (
        "history_page_bytes",
        "History page size (bytes)",
        "256–8000 bytes per original-history page.",
        "Memory retrieval",
    ),
    (
        "history_search_results",
        "History search results",
        "1–16 results per page.",
        "Memory retrieval",
    ),
    (
        "retrieval_records",
        "Evidence window",
        "1–256 recent records. Missing proof requires revalidation.",
        "Memory retrieval",
    ),
];

const SAVE: usize = FIELDS.len();
const RESET: usize = SAVE + 1;
const CANCEL: usize = SAVE + 2;
const ACTIONS: [(usize, &str); 3] = [
    (SAVE, "Save settings"),
    (RESET, "Restore defaults"),
    (CANCEL, "Cancel"),
];

/// Which part of the menu consumes keystrokes.
enum Focus {
    List,
    /// Narrowing the list by label or section.
    Filter,
    /// Typing a replacement number for the highlighted setting.
    Edit(String),
}

struct Menu {
    original: PipelineSettings,
    draft: PipelineSettings,
    selected: usize,
    focus: Focus,
    filter: String,
    note: String,
    alert: bool,
    scroll: usize,
}

enum Outcome {
    Continue,
    Save,
    Cancel,
}

impl Menu {
    fn new(settings: &PipelineSettings) -> Self {
        Self {
            original: settings.clone(),
            draft: settings.clone(),
            selected: 0,
            focus: Focus::List,
            filter: String::new(),
            note: String::new(),
            alert: false,
            scroll: 0,
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
    fn changed(&self) -> bool {
        self.draft != self.original
    }
    /// Setting indices matching the filter, with the three actions always last
    /// so Save and Cancel stay reachable however the list is narrowed.
    fn visible(&self) -> Vec<usize> {
        let filter = self.filter.trim().to_lowercase();
        let mut visible: Vec<usize> = (0..FIELDS.len())
            .filter(|index| {
                filter.is_empty()
                    || FIELDS[*index].1.to_lowercase().contains(&filter)
                    || FIELDS[*index].3.to_lowercase().contains(&filter)
            })
            .collect();
        visible.extend(ACTIONS.map(|(index, _)| index));
        visible
    }
    /// Menu rows plus, for each row, the setting or action it selects.
    fn rows(&self) -> (Vec<menu::Row>, Vec<Option<usize>>) {
        let (mut rows, mut targets) = (Vec::new(), Vec::new());
        let mut section = "";
        for index in self.visible() {
            let (label, value) = match ACTIONS.iter().find(|(action, _)| *action == index) {
                Some((SAVE, label)) => {
                    rows.push(menu::heading("Apply"));
                    targets.push(None);
                    if self.changed() {
                        (*label, Value::Text("unsaved changes".into()))
                    } else {
                        (*label, Value::Action)
                    }
                }
                Some((_, label)) => (*label, Value::Action),
                None => {
                    if FIELDS[index].3 != section {
                        section = FIELDS[index].3;
                        rows.push(menu::heading(section));
                        targets.push(None);
                    }
                    (FIELDS[index].1, self.painted(index))
                }
            };
            rows.push(menu::item(label, value));
            targets.push(Some(index));
        }
        (rows, targets)
    }
    fn painted(&self, index: usize) -> Value {
        if let (Focus::Edit(draft), true) = (&self.focus, index == self.selected) {
            return Value::Text(format!("{draft}_"));
        }
        match self.value(index).as_str() {
            "On" => Value::On,
            "Off" => Value::Off,
            number => Value::Number(number.into()),
        }
    }
    fn detail(&self) -> String {
        if !self.note.is_empty() {
            return self.note.clone();
        }
        match self.selected {
            SAVE if self.changed() => "Save writes every shown value to this profile and applies it to the running session.".into(),
            SAVE => "Nothing has changed yet; saving rewrites the same values.".into(),
            RESET => "Restore every setting to its built-in default in this menu. Nothing is written until you save.".into(),
            CANCEL => "Discard the draft and leave the saved settings untouched.".into(),
            index => FIELDS[index].2.into(),
        }
    }
    fn hint(&self) -> &'static str {
        match self.focus {
            Focus::Edit(_) => "type digits · enter accept · esc keep current",
            Focus::Filter => "type to narrow · enter list · esc clear filter",
            Focus::List => "↑↓ move · space toggle · enter change · / filter · s save · esc cancel",
        }
    }
    fn subtitle(&self, profile: &str) -> String {
        let scope = format!("profile · {}", safe(profile));
        if self.changed() {
            format!("{scope} · unsaved")
        } else {
            scope
        }
    }
    /// Move `steps` selectable entries through the filtered list, wrapping.
    fn step(&mut self, steps: isize) {
        let visible = self.visible();
        let at = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let len = visible.len();
        self.selected = visible[(at + len).saturating_add_signed(steps) % len];
    }
    fn clear_note(&mut self) {
        self.note.clear();
        self.alert = false;
    }
    fn fail(&mut self, message: impl Into<String>) {
        self.note = message.into();
        self.alert = true;
    }
    /// Keep the highlight on a visible entry after the filter changes.
    fn refocus(&mut self) {
        let visible = self.visible();
        if !visible.contains(&self.selected) {
            self.selected = visible[0];
        }
    }
    fn activate(&mut self) -> Outcome {
        self.clear_note();
        match self.selected {
            SAVE => return Outcome::Save,
            RESET => {
                self.draft = PipelineSettings::default();
                self.note = "Defaults restored in this menu; save to apply them.".into();
            }
            CANCEL => return Outcome::Cancel,
            index => {
                let value = self.value(index);
                if value == "On" || value == "Off" {
                    self.change(if value == "On" { "false" } else { "true" });
                } else {
                    self.focus = Focus::Edit(String::new());
                    self.note = format!("Current value: {value}. {}", FIELDS[index].2);
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
                self.focus = Focus::List;
                self.clear_note();
            }
            Err(error) => self.fail(error.to_string()),
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
        match &self.focus {
            Focus::Edit(_) => self.edit_key(key),
            Focus::Filter => self.filter_key(key),
            Focus::List => return self.list_key(key),
        }
        Outcome::Continue
    }
    fn edit_key(&mut self, key: KeyEvent) {
        let Focus::Edit(draft) = &mut self.focus else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.focus = Focus::List;
                self.clear_note();
            }
            KeyCode::Enter => {
                let value = draft.clone();
                if value.is_empty() {
                    self.focus = Focus::List;
                    self.clear_note();
                } else {
                    self.change(&value);
                }
            }
            KeyCode::Backspace => {
                draft.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() && draft.len() < 20 => draft.push(c),
            _ => {}
        }
    }
    fn filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.focus = Focus::List;
                self.refocus();
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Up => self.focus = Focus::List,
            KeyCode::Backspace => {
                self.filter.pop();
                self.refocus();
            }
            KeyCode::Char(c) if self.filter.len() < 40 => {
                self.filter.push(c);
                self.refocus();
            }
            _ => {}
        }
        if self.visible().len() == ACTIONS.len() && !self.filter.is_empty() {
            self.note = format!("No setting matches “{}”.", safe(&self.filter));
            self.alert = true;
        } else {
            self.clear_note();
        }
    }
    fn list_key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refocus();
            }
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Cancel,
            KeyCode::Up => self.step(-1),
            KeyCode::Down | KeyCode::Tab => self.step(1),
            KeyCode::PageUp => self.step(-10),
            KeyCode::PageDown => self.step(10),
            KeyCode::Home => self.selected = self.visible()[0],
            KeyCode::End => self.selected = SAVE,
            KeyCode::Char('/') => {
                self.focus = Focus::Filter;
                self.clear_note();
            }
            KeyCode::Char('s') => {
                self.selected = SAVE;
                return Outcome::Save;
            }
            KeyCode::Char('r') => {
                self.selected = RESET;
                return self.activate();
            }
            KeyCode::Enter | KeyCode::Char(' ') => return self.activate(),
            _ => {}
        }
        Outcome::Continue
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
    let _screen = Screen::enter()?;
    loop {
        let (rows, targets) = menu.rows();
        let selected = targets
            .iter()
            .position(|target| *target == Some(menu.selected))
            .unwrap_or(0);
        let (subtitle, detail, hint) = (menu.subtitle(profile), menu.detail(), menu.hint());
        let title = if menu.filter.is_empty() {
            "Pipeline settings".to_owned()
        } else {
            format!("Pipeline settings · filter: {}_", safe(&menu.filter))
        };
        menu::draw(
            &View {
                title: &title,
                subtitle: &subtitle,
                note: &detail,
                alert: menu.alert,
                hint,
                rows: &rows,
                selected,
            },
            &mut menu.scroll,
        )?;
        if let Event::Key(key) = event::read()? {
            match menu.key(key) {
                Outcome::Continue => {}
                Outcome::Cancel => return Ok(None),
                Outcome::Save => match save(&menu.draft) {
                    Ok(()) => return Ok(Some(menu.draft)),
                    Err(error) => menu.fail(error),
                },
            }
        }
    }
}

fn plain_menu(
    menu: &mut Menu,
    profile: &str,
    save: &mut impl FnMut(&PipelineSettings) -> Result<(), String>,
) -> io::Result<Option<PipelineSettings>> {
    let width = FIELDS
        .iter()
        .map(|(_, label, ..)| label.len())
        .max()
        .unwrap_or(0);
    loop {
        println!(
            "\nPipeline settings · {}\nSave applies to this session and its profile.",
            safe(profile)
        );
        let mut section = "";
        for (index, (_, label, _, group)) in FIELDS.iter().enumerate() {
            if *group != section {
                section = group;
                println!("\n  {section}");
            }
            println!("{:>3}. {label:width$}  {}", index + 1, menu.value(index));
        }
        println!("\ns. Save settings   r. Restore defaults   q. Cancel");
        if !menu.note.is_empty() {
            println!("{}", safe(&menu.note));
        }
        print!("Choose an item: ");
        io::stdout().flush()?;
        let Some(choice) = menu::read_choice()? else {
            return Ok(None);
        };
        match choice.as_str() {
            "q" | "" => return Ok(None),
            "s" => match save(&menu.draft) {
                Ok(()) => return Ok(Some(menu.draft.clone())),
                Err(error) => menu.fail(error),
            },
            "r" => {
                menu.draft = PipelineSettings::default();
                menu.note = "Defaults restored; Save to apply.".into();
            }
            _ => match choice.parse::<usize>() {
                Ok(index) if (1..=FIELDS.len()).contains(&index) => {
                    menu.selected = index - 1;
                    menu.activate();
                    if matches!(menu.focus, Focus::Edit(_)) {
                        println!("{}", FIELDS[index - 1].2);
                        print!("New value (Enter keeps current): ");
                        io::stdout().flush()?;
                        let Some(value) = menu::read_choice()? else {
                            return Ok(None);
                        };
                        if !value.is_empty() {
                            menu.change(&value);
                        }
                        menu.focus = Focus::List;
                    }
                }
                Ok(_) => menu.fail("Choose a listed item."),
                Err(_) => menu.fail("Choose an item number, s, r or q."),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn typed(menu: &mut Menu, text: &str) {
        for c in text.chars() {
            menu.key(key(KeyCode::Char(c)));
        }
    }
    #[test]
    fn menu_covers_every_setting_and_cancel_does_not_change_original() {
        let original = PipelineSettings::default();
        let fields = serde_json::to_value(&original).unwrap();
        assert_eq!(FIELDS.len(), fields.as_object().unwrap().len());
        for (key, ..) in FIELDS {
            assert!(fields.get(*key).is_some(), "{key} is not a setting");
        }
        let mut menu = Menu::new(&original);
        menu.key(key(KeyCode::Enter));
        assert!(!menu.draft.enabled);
        assert!(matches!(menu.key(key(KeyCode::Esc)), Outcome::Cancel));
        assert!(original.enabled);
    }
    #[test]
    fn every_section_is_one_contiguous_group_so_headings_are_not_repeated() {
        let mut seen: Vec<&str> = vec![];
        for (.., section) in FIELDS {
            if seen.last() != Some(section) {
                assert!(!seen.contains(section), "{section} is split in two");
                seen.push(section);
            }
        }
        let (rows, targets) = Menu::new(&PipelineSettings::default()).rows();
        assert_eq!(rows.len(), FIELDS.len() + ACTIONS.len() + seen.len() + 1);
        assert_eq!(
            targets.iter().flatten().count(),
            FIELDS.len() + ACTIONS.len()
        );
    }
    #[test]
    fn number_editor_validates_and_navigation_reaches_save() {
        let mut menu = Menu::new(&PipelineSettings::default());
        menu.selected = FIELDS
            .iter()
            .position(|(key, ..)| *key == "candidate_attempts")
            .unwrap();
        menu.activate();
        menu.key(key(KeyCode::Char('0')));
        menu.key(key(KeyCode::Enter));
        assert!(matches!(menu.focus, Focus::Edit(_)));
        assert!(
            menu.alert,
            "an out-of-range value is reported: {}",
            menu.note
        );
        assert_eq!(menu.draft.candidate_attempts, 3);
        menu.key(key(KeyCode::Backspace));
        menu.key(key(KeyCode::Char('5')));
        menu.key(key(KeyCode::Enter));
        assert_eq!(menu.draft.candidate_attempts, 5);
        assert!(matches!(menu.focus, Focus::List));
        menu.key(key(KeyCode::End));
        assert!(matches!(menu.key(key(KeyCode::Enter)), Outcome::Save));
        menu.selected = RESET;
        menu.activate();
        assert_eq!(menu.draft, PipelineSettings::default());
    }
    #[test]
    fn the_filter_narrows_the_list_and_always_keeps_the_actions_reachable() {
        let mut menu = Menu::new(&PipelineSettings::default());
        menu.key(key(KeyCode::Char('/')));
        typed(&mut menu, "subagent");
        let visible = menu.visible();
        assert_eq!(visible.len(), 3 + ACTIONS.len(), "{visible:?}");
        assert!(visible.contains(&SAVE) && visible.contains(&CANCEL));
        assert!(menu.visible().contains(&menu.selected));
        // Stepping wraps through the matches and the actions, never a hidden row.
        for _ in 0..visible.len() + 1 {
            menu.step(1);
            assert!(visible.contains(&menu.selected));
        }
        typed(&mut menu, "zzz");
        assert!(menu.alert, "an empty result is reported");
        assert_eq!(menu.selected, SAVE);
        menu.key(key(KeyCode::Esc));
        assert!(menu.filter.is_empty());
        assert_eq!(menu.visible().len(), FIELDS.len() + ACTIONS.len());
    }
    #[test]
    fn list_shortcuts_save_reset_and_report_unsaved_changes() {
        let mut menu = Menu::new(&PipelineSettings::default());
        assert!(!menu.changed());
        menu.key(key(KeyCode::Char(' ')));
        assert!(menu.changed());
        assert!(menu.subtitle("local").ends_with("· unsaved"));
        assert!(matches!(
            menu.key(key(KeyCode::Char('r'))),
            Outcome::Continue
        ));
        assert!(!menu.changed());
        assert!(matches!(menu.key(key(KeyCode::Char('s'))), Outcome::Save));
        // A letter typed into the filter is text, not a shortcut.
        menu.key(key(KeyCode::Char('/')));
        typed(&mut menu, "s");
        assert_eq!(menu.filter, "s");
    }
}

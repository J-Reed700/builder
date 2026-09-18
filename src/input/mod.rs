//! Inline terminal composer: bracketed paste, bounded viewport, and event bursts.
//! No alternate screen: normal terminal scrollback and text selection remain.
pub mod buffer;
mod clipboard;
pub mod layout;
pub mod memory;
pub mod menu;
pub mod pipeline;

use crate::ui::{safe, theme};

/// Source comments and absolute paths are prompts, not slash commands.
pub fn is_unknown_command(text: &str) -> bool {
    !text.contains(char::is_whitespace)
        && text
            .strip_prefix('/')
            .is_some_and(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphabetic()))
}
use buffer::{Buffer, Draft};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
mod screen;
use layout::Layout;
use screen::Screen;
use std::{
    collections::VecDeque,
    io::{self, Write},
    time::{Duration, Instant},
};

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Keyboard shortcuts and commands"),
    ("/status", "Context usage and session details"),
    ("/settings", "Pipeline features and budgets"),
    ("/memory", "Local memory settings and model setup"),
    ("/history", "Show the saved conversation"),
    ("/todo", "Show the agent's current todo list"),
    ("/retry", "Continue an unfinished turn"),
    ("/compact", "Summarize context; preserve original history"),
    ("/cancel", "Cancel pending work and keep the conversation"),
    ("/rewind", "Edit the previous message; archive its turn"),
    (
        "/clear",
        "Start a fresh conversation; archive the current one",
    ),
    ("/attach ", "Attach a file from this workspace"),
    ("/exit", "Save and leave"),
];

pub enum Input {
    Submit(String),
    Exit,
}
enum Outcome {
    Continue,
    Clipboard,
    Submit,
    Exit,
}

#[derive(Default)]
pub struct Composer {
    buffer: Buffer,
    history: VecDeque<Draft>,
    history_index: Option<usize>,
    saved_draft: Draft,
    note: String,
    completion: usize,
    menu_dismissed: bool,
    history_bytes: usize,
}

impl Composer {
    /// Restore a saved prompt without submitting it. Keep ordinary prompts
    /// editable, and fold very large transcripts using the normal paste path.
    pub fn set_draft(&mut self, text: &str) -> io::Result<()> {
        let mut buffer = Buffer::default();
        buffer
            .insert(text, text.len() > 16 * 1024)
            .map_err(io::Error::other)?;
        self.buffer = buffer;
        self.history_index = None;
        Ok(())
    }
    pub fn read_plain(&mut self) -> io::Result<Input> {
        if !self.buffer.is_empty() {
            println!(
                "Restored draft (Enter resends it; type a replacement to edit):\n{}",
                safe(&self.buffer.content())
            );
        }
        print!("builder › ");
        io::stdout().flush()?;
        let mut text = String::new();
        if io::stdin().read_line(&mut text)? == 0 {
            return Ok(Input::Exit);
        }
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        if text.is_empty() && !self.buffer.is_empty() {
            text = self.buffer.content();
        }
        self.buffer.restore(Draft::default());
        Ok(Input::Submit(text))
    }
    pub fn read(&mut self, status: &str) -> io::Result<Input> {
        let mut screen = Screen::enter()?;
        self.note.clear();
        self.history_index = None;
        self.menu_dismissed = false;
        self.completion = 0;
        screen.draw(&self.buffer, status, &self.hint(), &self.menu())?;
        loop {
            let event = event::read()?;
            let start = Instant::now();
            let mut outcome = self.handle(event, screen.content_width);
            // Drain an input burst within one frame, avoiding a full repaint per
            // keystroke. A bracketed paste is one insertion and one repaint.
            while matches!(outcome, Outcome::Continue)
                && start.elapsed() < Duration::from_millis(8)
                && event::poll(Duration::ZERO)?
            {
                outcome = self.handle(event::read()?, screen.content_width);
            }
            match outcome {
                Outcome::Clipboard => {
                    self.note = "Reading clipboard directly…".into();
                    screen.draw(&self.buffer, status, &self.hint(), &self.menu())?;
                    match clipboard::read(buffer::MAX_INPUT_BYTES - self.buffer.draft.bytes) {
                        Ok(text) if text.is_empty() => {
                            self.note = "Clipboard is empty or contains no text".into();
                        }
                        Ok(text) => {
                            self.handle(Event::Paste(text), screen.content_width);
                        }
                        Err(error) => self.note = error.to_string(),
                    }
                    screen.draw(&self.buffer, status, &self.hint(), &self.menu())?;
                }
                Outcome::Submit => {
                    let text = self.buffer.content();
                    if text.trim().is_empty() {
                        continue;
                    }
                    screen.clear()?;
                    let preview_width = screen.content_width;
                    drop(screen);
                    let label = if self.buffer.pastes() > 0 {
                        format!(
                            "{} · {} paste blocks",
                            buffer::size(text.len()),
                            self.buffer.pastes()
                        )
                    } else {
                        String::new()
                    };
                    let visible = self
                        .buffer
                        .draft
                        .atoms
                        .iter()
                        .map(|atom| atom.display())
                        .collect::<String>();
                    let preview = layout::wrap(&visible, preview_width);
                    println!(
                        "\n  {} {}",
                        theme::accent("›"),
                        theme::title(&if label.is_empty() {
                            "You".into()
                        } else {
                            format!("You · {label}")
                        })
                    );
                    for line in preview.iter().take(6) {
                        println!("    {}", safe(line));
                    }
                    if preview.len() > 6 {
                        println!(
                            "    {}",
                            theme::muted(&format!(
                                "… {} more lines · full prompt saved",
                                preview.len() - 6
                            ))
                        );
                    }
                    self.remember();
                    self.buffer.restore(Draft::default());
                    return Ok(Input::Submit(text));
                }
                Outcome::Exit => {
                    screen.clear()?;
                    return Ok(Input::Exit);
                }
                Outcome::Continue => {
                    screen.draw(&self.buffer, status, &self.hint(), &self.menu())?;
                }
            }
        }
    }
    fn remember(&mut self) {
        let draft = self.buffer.draft.clone();
        self.history_bytes += draft.bytes;
        self.history.push_back(draft);
        while self.history.len() > 100 || self.history_bytes > 16 * 1024 * 1024 {
            if let Some(old) = self.history.pop_front() {
                self.history_bytes -= old.bytes;
            }
        }
    }
    fn history(&mut self, previous: bool) {
        if previous {
            let index = match self.history_index {
                None => {
                    self.saved_draft = self.buffer.draft.clone();
                    self.history.len().checked_sub(1)
                }
                Some(index) => index.checked_sub(1),
            };
            if let Some(index) = index {
                self.buffer.restore(self.history[index].clone());
                self.history_index = Some(index);
            }
        } else if let Some(index) = self.history_index {
            if index + 1 == self.history.len() {
                self.buffer.restore(self.saved_draft.clone());
                self.history_index = None;
            } else {
                self.buffer.restore(self.history[index + 1].clone());
                self.history_index = Some(index + 1);
            }
        }
    }
    fn suggestions(&self) -> Vec<(&'static str, &'static str)> {
        if self.menu_dismissed || self.buffer.draft.bytes > 32 || self.buffer.pastes() > 0 {
            return vec![];
        }
        let input = self.buffer.content();
        if !input.starts_with('/') {
            return vec![];
        }
        COMMANDS
            .iter()
            .copied()
            .filter(|(command, _)| command.starts_with(input.as_str()))
            .collect()
    }
    fn menu(&self) -> Vec<String> {
        let options = self.suggestions();
        if options.is_empty() {
            return vec![];
        }
        let selected = self.completion % options.len();
        let start = selected
            .saturating_sub(1)
            .min(options.len().saturating_sub(3));
        options
            .iter()
            .enumerate()
            .skip(start)
            .take(3)
            .map(|(index, (command, description))| {
                format!(
                    "{} {command:<10} {description}",
                    if index == selected { "›" } else { " " }
                )
            })
            .collect()
    }
    fn hint(&self) -> String {
        if !self.note.is_empty() {
            return self.note.clone();
        }
        let suggestions = self.suggestions();
        if !suggestions.is_empty() {
            let selected = self.completion % suggestions.len();
            let (command, _) = suggestions[selected];
            let action = if self.buffer.content() == command {
                "run"
            } else {
                "choose"
            };
            return format!(
                "{command}  ·  ↑↓ {}/{}  ·  enter {action}  ·  tab complete  ·  esc close",
                selected + 1,
                suggestions.len()
            );
        }
        if let Some(index) = self.history_index {
            return format!(
                "History {}/{}  ·  ↓ newer / restore draft  ·  enter send",
                index + 1,
                self.history.len()
            );
        }
        if self.buffer.is_empty() {
            "/ commands  ·  ↑ history".into()
        } else {
            "enter send  ·  alt+enter newline".into()
        }
    }
    fn complete(&mut self) {
        let options = self.suggestions();
        if !options.is_empty() {
            let command = options[self.completion % options.len()].0;
            self.buffer.clear();
            self.insert(command);
            self.completion = 0;
        }
    }
    fn handle(&mut self, event: Event, width: usize) -> Outcome {
        match event {
            Event::Paste(text) => {
                self.completion = 0;
                self.menu_dismissed = false;
                self.note = match self.buffer.insert(&text, true) {
                    Ok(()) => format!(
                        "{} pasted · full text retained · enter to send · ctrl+z undo",
                        buffer::size(text.len())
                    ),
                    Err(error) => error.into(),
                };
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.note.clear();
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.insert("\n")
                    }
                    KeyCode::Enter => {
                        let options = self.suggestions();
                        if !options.is_empty()
                            && self.buffer.content() != options[self.completion % options.len()].0
                        {
                            self.complete();
                        } else {
                            return Outcome::Submit;
                        }
                    }
                    KeyCode::Char('j') if ctrl => self.insert("\n"),
                    KeyCode::Char('v') if ctrl => return Outcome::Clipboard,
                    KeyCode::Char('c') if ctrl => {
                        if self.buffer.is_empty() {
                            return Outcome::Exit;
                        }
                        self.buffer.clear();
                        self.note =
                            "Draft cleared · ctrl+z restores it · ctrl+c again exits".into();
                    }
                    KeyCode::Char('d') if ctrl && self.buffer.is_empty() => return Outcome::Exit,
                    KeyCode::Char('a') if ctrl => self.buffer.home(),
                    KeyCode::Char('e') if ctrl => self.buffer.end(),
                    KeyCode::Char('u') if ctrl => self.buffer.clear(),
                    KeyCode::Char('w') if ctrl => self.buffer.delete_word(),
                    KeyCode::Char('z') if ctrl => self.buffer.undo(),
                    KeyCode::Char('y') if ctrl => self.buffer.redo(),
                    KeyCode::Char('p') if ctrl => self.history(true),
                    KeyCode::Char('n') if ctrl => self.history(false),
                    KeyCode::Char(c) if !ctrl && !alt => self.insert(&c.to_string()),
                    KeyCode::Left => self.buffer.left(),
                    KeyCode::Right => self.buffer.right(),
                    KeyCode::Home => self.buffer.home(),
                    KeyCode::End => self.buffer.end(),
                    KeyCode::Backspace => self.buffer.backspace(),
                    KeyCode::Delete => self.buffer.delete(),
                    KeyCode::Up | KeyCode::Down if !self.suggestions().is_empty() => {
                        let count = self.suggestions().len();
                        self.completion = if key.code == KeyCode::Down {
                            (self.completion + 1) % count
                        } else {
                            (self.completion + count - 1) % count
                        };
                    }
                    KeyCode::Up | KeyCode::Down => {
                        let layout = Layout::new(&self.buffer, width);
                        let down = key.code == KeyCode::Down;
                        if (!down && layout.cursor.0 == 0)
                            || (down && layout.cursor.0 + 1 == layout.lines.len())
                        {
                            if self.buffer.is_empty() || self.history_index.is_some() {
                                self.history(!down);
                            }
                        } else {
                            self.buffer.draft.cursor =
                                layout.vertical(self.buffer.draft.cursor, down);
                        }
                    }
                    KeyCode::Tab => self.complete(),
                    KeyCode::BackTab => {
                        let count = self.suggestions().len();
                        if count > 0 {
                            self.completion = (self.completion + 1) % count;
                        }
                    }
                    KeyCode::Esc => {
                        self.menu_dismissed = true;
                        self.note = "ctrl+u clear · ctrl+z undo · ctrl+p / ctrl+n history".into();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Outcome::Continue
    }
    fn insert(&mut self, text: &str) {
        self.completion = 0;
        self.menu_dismissed = false;
        if let Err(error) = self.buffer.insert(text, false) {
            self.note = error.into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn command_menu_browses_completes_then_submits() {
        let mut editor = Composer::default();
        editor.insert("/");
        assert_eq!(editor.menu().len(), 3);
        editor.handle(key(KeyCode::Down, KeyModifiers::NONE), 80);
        assert!(
            editor
                .menu()
                .iter()
                .any(|line| line.starts_with("› /status"))
        );
        assert!(matches!(
            editor.handle(key(KeyCode::Enter, KeyModifiers::NONE), 80),
            Outcome::Continue
        ));
        assert_eq!(editor.buffer.content(), "/status");
        assert!(matches!(
            editor.handle(key(KeyCode::Enter, KeyModifiers::NONE), 80),
            Outcome::Submit
        ));
    }

    #[test]
    fn command_menu_wraps_and_escape_preserves_draft() {
        let mut editor = Composer::default();
        editor.insert("/");
        editor.handle(key(KeyCode::Up, KeyModifiers::NONE), 80);
        assert!(editor.menu().iter().any(|line| line.starts_with("› /exit")));
        editor.handle(key(KeyCode::Esc, KeyModifiers::NONE), 80);
        assert!(editor.menu().is_empty());
        assert_eq!(editor.buffer.content(), "/");
        editor.insert("sta");
        assert!(editor.menu()[0].starts_with("› /status"));
    }

    #[test]
    fn arrow_navigation_preserves_draft_at_boundaries() {
        let mut editor = Composer::default();
        editor.insert("previous");
        editor.remember();
        editor.set_draft("draft").unwrap();
        editor.handle(key(KeyCode::Up, KeyModifiers::NONE), 80);
        assert_eq!(editor.buffer.content(), "draft");
        editor.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL), 80);
        assert_eq!(editor.buffer.content(), "previous");
        editor.handle(key(KeyCode::Down, KeyModifiers::NONE), 80);
        assert_eq!(editor.buffer.content(), "draft");
    }

    #[test]
    fn restored_prompt_is_editable_and_never_auto_submits() {
        let mut editor = Composer::default();
        editor.set_draft("write code").unwrap();
        editor.handle(key(KeyCode::Backspace, KeyModifiers::NONE), 80);
        editor.handle(Event::Paste(" instead".into()), 80);
        assert_eq!(editor.buffer.content(), "write cod instead");
        assert!(
            editor
                .set_draft(&"x".repeat(buffer::MAX_INPUT_BYTES + 1))
                .is_err()
        );
        assert_eq!(editor.buffer.content(), "write cod instead");
        let large = "line\r\n".repeat(10000);
        editor.set_draft(&large).unwrap();
        assert_eq!(editor.buffer.pastes(), 1);
        assert_eq!(editor.buffer.content(), large);
    }

    #[test]
    fn direct_paste_requests_clipboard_without_submitting_or_changing_draft() {
        let mut editor = Composer::default();
        editor.buffer.insert("explain: ", false).unwrap();
        assert!(matches!(
            editor.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL), 80),
            Outcome::Clipboard
        ));
        assert_eq!(editor.buffer.content(), "explain: ");
        editor.handle(Event::Paste("/exit\r\n/retry\n".into()), 80);
        assert_eq!(editor.buffer.content(), "explain: /exit\r\n/retry\n");
        editor.buffer.undo();
        assert_eq!(editor.buffer.content(), "explain: ");
    }

    #[test]
    fn bracketed_paste_never_submits_even_if_it_contains_commands() {
        let mut editor = Composer::default();
        assert!(matches!(
            editor.handle(Event::Paste("/exit\n/retry\n".into()), 80),
            Outcome::Continue
        ));
        assert_eq!(editor.buffer.content(), "/exit\n/retry\n");
    }

    #[test]
    fn history_navigation_restores_an_unsent_draft() {
        let mut editor = Composer::default();
        editor.buffer.insert("previous task", false).unwrap();
        editor.remember();
        editor.buffer.restore(Draft::default());
        editor.buffer.insert("unsent draft", false).unwrap();
        editor.history(true);
        assert_eq!(editor.buffer.content(), "previous task");
        editor.history(false);
        assert_eq!(editor.buffer.content(), "unsent draft");
    }

    #[test]
    fn tab_completes_commands_and_ctrl_j_inserts_newline() {
        let mut editor = Composer::default();
        editor.buffer.insert("/sta", false).unwrap();
        editor.handle(key(KeyCode::Tab, KeyModifiers::NONE), 80);
        assert_eq!(editor.buffer.content(), "/status");
        assert!(matches!(
            editor.handle(key(KeyCode::Char('j'), KeyModifiers::CONTROL), 80),
            Outcome::Continue
        ));
        assert_eq!(editor.buffer.content(), "/status\n");
    }

    #[test]
    fn control_c_clears_draft_with_undo_before_exiting() {
        let mut editor = Composer::default();
        editor.buffer.insert("keep me", false).unwrap();
        assert!(matches!(
            editor.handle(key(KeyCode::Char('c'), KeyModifiers::CONTROL), 80),
            Outcome::Continue
        ));
        assert!(editor.buffer.is_empty());
        editor.buffer.undo();
        assert_eq!(editor.buffer.content(), "keep me");
    }
}

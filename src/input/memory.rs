//! The `/memory` chooser. It only reports which mode the user picked; enabling a
//! backend, downloading a model and saving configuration stay in the app, so a
//! long setup runs with the menu closed and normal progress output visible.
use super::menu::{self, Screen, Value, View};
use crate::ui::safe;
use builder_core::config::{EmbeddingBackend, MemorySettings};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::io::{self, Write};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Local,
    Lexical,
    Disabled,
}

const MODES: &[(Mode, &str, &str)] = &[
    (
        Mode::Local,
        "Local embeddings",
        "Semantic and keyword recall on this machine. One-time ~91 MB model download, then fully offline.",
    ),
    (
        Mode::Lexical,
        "Keyword-only memory",
        "Keyword recall with no embedding model. Nothing is downloaded and no vectors are stored.",
    ),
    (
        Mode::Disabled,
        "Memory off",
        "Stop recalling and recording findings. Stored records are retained and return if you re-enable memory.",
    ),
];

/// The mode a configuration is already using, so the menu can mark it.
fn active(settings: &MemorySettings) -> Option<Mode> {
    if !settings.enabled {
        return Some(Mode::Disabled);
    }
    match settings.embedding_backend {
        EmbeddingBackend::Local => Some(Mode::Local),
        EmbeddingBackend::Lexical => Some(Mode::Lexical),
        EmbeddingBackend::Remote => None,
    }
}

fn summary(settings: &MemorySettings) -> String {
    match active(settings) {
        Some(Mode::Local) => "now: local embeddings".into(),
        Some(Mode::Lexical) => "now: keyword only".into(),
        Some(Mode::Disabled) => "now: off".into(),
        None => "now: remote embeddings".into(),
    }
}

/// Returns the chosen mode, or `None` when the user cancelled.
pub fn choose(settings: &MemorySettings, plain: bool) -> io::Result<Option<Mode>> {
    if plain {
        return plain_menu(settings);
    }
    let current = active(settings);
    let rows: Vec<_> = MODES
        .iter()
        .map(|(mode, label, _)| {
            menu::item(
                *label,
                if Some(*mode) == current {
                    Value::Text("current".into())
                } else {
                    Value::Action
                },
            )
        })
        .collect();
    let _screen = Screen::enter()?;
    let mut selected = current
        .and_then(|mode| MODES.iter().position(|(candidate, ..)| *candidate == mode))
        .unwrap_or(0);
    let mut scroll = 0;
    loop {
        menu::draw(
            &View {
                title: "Memory",
                subtitle: &summary(settings),
                note: MODES[selected].2,
                alert: false,
                hint: "↑↓ choose · 1–3 jump · enter apply · esc cancel",
                rows: &rows,
                selected,
            },
            &mut scroll,
        )?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match action(key, MODES.len()) {
            Action::Move(delta) => {
                selected = (selected + MODES.len()).saturating_add_signed(delta) % MODES.len()
            }
            Action::Jump(index) => selected = index,
            Action::Accept => return Ok(Some(MODES[selected].0)),
            Action::Cancel => return Ok(None),
            Action::None => {}
        }
    }
}

enum Action {
    Move(isize),
    Jump(usize),
    Accept,
    Cancel,
    None,
}

fn action(key: KeyEvent, len: usize) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'd'))
    {
        return Action::Cancel;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => Action::Cancel,
        KeyCode::Up | KeyCode::Char('k') => Action::Move(-1),
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => Action::Move(1),
        KeyCode::Enter | KeyCode::Char(' ') => Action::Accept,
        // A digit jumps to that option; Enter still confirms it.
        KeyCode::Char(c) if c.is_ascii_digit() => match c.to_digit(10).unwrap_or(0) as usize {
            choice if (1..=len).contains(&choice) => Action::Jump(choice - 1),
            _ => Action::None,
        },
        _ => Action::None,
    }
}

fn plain_menu(settings: &MemorySettings) -> io::Result<Option<Mode>> {
    println!("\nMemory settings · {}", summary(settings));
    for (index, (mode, label, detail)) in MODES.iter().enumerate() {
        println!(
            "{}. {label}{}\n   {detail}",
            index + 1,
            if active(settings) == Some(*mode) {
                " (current)"
            } else {
                ""
            }
        );
    }
    print!("Choose 1, 2 or 3 (Enter cancels): ");
    io::stdout().flush()?;
    match menu::read_choice()?.as_deref() {
        Some("1") => Ok(Some(Mode::Local)),
        Some("2") => Ok(Some(Mode::Lexical)),
        Some("3") => Ok(Some(Mode::Disabled)),
        None | Some("") => Ok(None),
        Some(other) => Err(io::Error::other(format!(
            "Choose 1, 2 or 3; memory settings unchanged (got {})",
            safe(other)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_configured_mode_is_reported_for_every_backend() {
        let mut settings = MemorySettings {
            enabled: true,
            ..MemorySettings::default()
        };
        settings.embedding_backend = EmbeddingBackend::Local;
        assert_eq!(active(&settings), Some(Mode::Local));
        settings.embedding_backend = EmbeddingBackend::Lexical;
        assert_eq!(active(&settings), Some(Mode::Lexical));
        settings.embedding_backend = EmbeddingBackend::Remote;
        assert_eq!(active(&settings), None);
        assert_eq!(summary(&settings), "now: remote embeddings");
        settings.enabled = false;
        assert_eq!(active(&settings), Some(Mode::Disabled));
    }

    #[test]
    fn digits_jump_to_an_option_and_arrows_wrap() {
        assert!(matches!(
            action(key(KeyCode::Char('2')), 3),
            Action::Jump(1)
        ));
        assert!(matches!(action(key(KeyCode::Char('9')), 3), Action::None));
        assert!(matches!(action(key(KeyCode::Up), 3), Action::Move(-1)));
        assert!(matches!(action(key(KeyCode::Esc), 3), Action::Cancel));
        assert!(matches!(action(key(KeyCode::Enter), 3), Action::Accept));
        assert!(matches!(
            action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 3),
            Action::Cancel
        ));
    }
}

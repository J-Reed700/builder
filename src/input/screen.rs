//! Owns terminal modes and repaints only the bounded composer region.
use super::{
    buffer::{self, Buffer},
    layout::{Layout, clip},
};
use crate::ui::{safe, theme};
use crossterm::{
    cursor,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    terminal::{self, Clear, ClearType, DisableLineWrap, EnableLineWrap},
};
use std::io::{self, Write};

pub(super) struct Screen {
    row: u16,
    rows: u16,
    pub(super) width: u16,
    pub(super) content_width: usize,
}
impl Screen {
    pub(super) fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // Construct the guard before fallible setup, so errors restore raw mode.
        let mut screen = Self {
            row: 0,
            rows: 0,
            width: 80,
            content_width: 71,
        };
        execute!(io::stdout(), EnableBracketedPaste, DisableLineWrap)?;
        let (width, height) = terminal::size()?;
        screen.width = width.clamp(1, 100);
        screen.content_width = content_width(width, height);
        screen.rows = height.saturating_sub(1).clamp(1, 8);
        for _ in 0..screen.rows {
            write!(io::stdout(), "\r\n")?;
        }
        execute!(
            io::stdout(),
            cursor::MoveUp(screen.rows),
            cursor::MoveToColumn(0)
        )?;
        Ok(screen)
    }
    pub(super) fn clear(&mut self) -> io::Result<()> {
        let mut out = io::stdout().lock();
        queue!(out, cursor::MoveToColumn(0))?;
        if self.row > 0 {
            queue!(out, cursor::MoveUp(self.row))?;
        }
        queue!(out, Clear(ClearType::FromCursorDown))?;
        self.row = 0;
        out.flush()
    }
    pub(super) fn draw(
        &mut self,
        buffer: &Buffer,
        status: &str,
        hint: &str,
        menu: &[String],
    ) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        self.width = width.clamp(1, 100);
        self.content_width = content_width(width, height);
        self.rows = height.saturating_sub(1).clamp(1, 8);
        let (lines, target_row, target_column) = if width < 24 || height < 7 {
            let content_width = self.content_width;
            let layout = Layout::new(buffer, content_width);
            let text = if buffer.is_empty() {
                "Ask Builder…"
            } else {
                &layout.lines[layout.cursor.0]
            };
            let mut lines = vec![format!(
                "{} {}",
                theme::accent("›"),
                clip(text, content_width)
            )];
            if height >= 3 {
                lines.push(theme::muted(&clip(
                    hint,
                    self.width.saturating_sub(1) as usize,
                )));
            }
            if height >= 4 {
                lines.push(theme::muted(&clip(
                    status,
                    self.width.saturating_sub(1) as usize,
                )));
            }
            (
                lines,
                0,
                (layout.cursor.1 as u16 + 2).min(self.width.saturating_sub(1)),
            )
        } else {
            let content_width = self.content_width;
            let layout = Layout::new(buffer, content_width);
            let menu_rows = menu.len().min((self.rows as usize).saturating_sub(5));
            let visible = layout
                .lines
                .len()
                .min((self.rows as usize).saturating_sub(4 + menu_rows).max(1));
            let offset = layout.cursor.0.saturating_sub(visible - 1);
            let border_width = self.width.saturating_sub(6) as usize;
            let mut lines = vec![format!(
                "  {} {}",
                theme::accent("Message"),
                theme::border(&"─".repeat(border_width.saturating_sub(6)))
            )];
            for index in 0..visible {
                let text = layout.lines.get(offset + index).map_or("", String::as_str);
                let plain = if buffer.is_empty() && index == 0 {
                    clip("Ask Builder…", content_width)
                } else {
                    safe(text)
                };
                let body = if buffer.is_empty() {
                    theme::muted(&plain)
                } else {
                    paint_pastes(&plain)
                };
                lines.push(format!(
                    "    {}{}",
                    if offset + index == layout.cursor.0 {
                        theme::accent("› ")
                    } else {
                        "  ".into()
                    },
                    body
                ));
            }
            let detail = if layout.lines.len() > visible {
                format!(
                    " lines {}–{} of {} · {} ",
                    offset + 1,
                    (offset + visible).min(layout.lines.len()),
                    layout.lines.len(),
                    buffer::size(buffer.draft.bytes)
                )
            } else if buffer.is_empty() {
                String::new()
            } else if buffer.pastes() > 0 || buffer.draft.bytes >= 1024 {
                format!(" {} ", buffer::size(buffer.draft.bytes))
            } else {
                String::new()
            };
            lines.push(composer_rule(&detail, border_width + 2));
            let selected = menu
                .iter()
                .position(|item| item.starts_with('›'))
                .unwrap_or(0);
            let menu_start = selected.saturating_sub(menu_rows.saturating_sub(1));
            for item in menu.iter().skip(menu_start).take(menu_rows) {
                let item = clip(&safe(item), self.width.saturating_sub(4) as usize);
                lines.push(format!(
                    "  {}",
                    if item.starts_with('›') {
                        theme::selected(&format!(
                            "{item}{}",
                            " ".repeat((self.width.saturating_sub(4) as usize).saturating_sub(
                                unicode_width::UnicodeWidthStr::width(item.as_str())
                            ))
                        ))
                    } else {
                        theme::muted(&item)
                    }
                ));
            }
            lines.push(format!(
                "  {}",
                paint_hint(&clip(&safe(hint), self.width.saturating_sub(4) as usize))
            ));
            lines.push(format!(
                "  {}",
                paint_status(&clip(&safe(status), self.width.saturating_sub(4) as usize))
            ));
            (
                lines,
                (layout.cursor.0 - offset + 1) as u16,
                (layout.cursor.1 + 6) as u16,
            )
        };
        let mut out = io::stdout().lock();
        // One synchronized write per input burst; terminals without support
        // safely ignore synchronized-update mode.
        queue!(
            out,
            terminal::BeginSynchronizedUpdate,
            cursor::Hide,
            cursor::MoveToColumn(0)
        )?;
        if self.row > 0 {
            queue!(out, cursor::MoveUp(self.row))?;
        }
        queue!(out, Clear(ClearType::FromCursorDown))?;
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                write!(out, "\r\n")?;
            }
            write!(out, "{line}")?;
        }
        self.row = target_row;
        let up = (lines.len() as u16 - 1).saturating_sub(self.row);
        if up > 0 {
            queue!(out, cursor::MoveUp(up))?;
        }
        queue!(
            out,
            cursor::MoveToColumn(target_column),
            cursor::Show,
            terminal::EndSynchronizedUpdate
        )?;
        out.flush()
    }
}

fn content_width(width: u16, height: u16) -> usize {
    let inset = if width < 24 || height < 7 { 3 } else { 9 };
    width.clamp(1, 100).saturating_sub(inset).max(1) as usize
}

fn composer_rule(label: &str, width: usize) -> String {
    let label = clip(label, width);
    let cells = unicode_width::UnicodeWidthStr::width(label.as_str());
    format!(
        "  {}{}",
        theme::border(&"─".repeat(width.saturating_sub(cells))),
        theme::muted(&label)
    )
}

fn paint_status(status: &str) -> String {
    let (state, details) = status.split_once(" · ").unwrap_or((status, ""));
    let state = if state.starts_with("Paused") {
        theme::warning(state)
    } else if state == "Saved" {
        theme::success(state)
    } else {
        theme::muted(state)
    };
    format!(
        "{state}{}",
        theme::muted(&if details.is_empty() {
            String::new()
        } else {
            format!(" · {details}")
        })
    )
}

fn paint_hint(hint: &str) -> String {
    hint.split("  ·  ")
        .map(|part| {
            if let Some((key, description)) = part.split_once(' ') {
                format!("{} {}", theme::title(key), theme::muted(description))
            } else {
                theme::muted(part)
            }
        })
        .collect::<Vec<_>>()
        .join(&theme::border("  ·  "))
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            terminal::EndSynchronizedUpdate,
            cursor::Show,
            EnableLineWrap,
            DisableBracketedPaste
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn paint_pastes(text: &str) -> String {
    let mut rest = text;
    let mut output = String::new();
    while let Some(start) = rest.find("[paste ") {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(']') else {
            break;
        };
        output.push_str(&theme::paste(&rest[..=end]));
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

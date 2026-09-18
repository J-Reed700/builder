//! Shared chrome for full-screen menus: the alternate-screen guard, sectioned
//! rows with an aligned value column, a footer that explains the highlighted
//! row, and the bounded line reader used by the plain-terminal fallbacks.
use super::layout::{clip, ellipsize, wrap};
use crate::ui::{safe, theme};
use crossterm::{
    cursor, execute, queue,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, BufRead, Write};
use unicode_width::UnicodeWidthStr;

/// Raw mode and the alternate screen are restored on every exit path, including
/// an error unwinding out of the menu loop.
pub struct Screen;
impl Screen {
    pub fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // Construct the guard before fallible setup, so errors restore raw mode.
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(guard)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub enum Row {
    Heading(String),
    Item { label: String, value: Value },
}

/// How a row's value is painted. Menus stay legible without color, so the text
/// itself distinguishes states rather than color alone.
pub enum Value {
    On,
    Off,
    Number(String),
    Text(String),
    Action,
}

impl Value {
    fn text(&self) -> &str {
        match self {
            Self::On => "On",
            Self::Off => "Off",
            Self::Number(value) | Self::Text(value) => value,
            Self::Action => "",
        }
    }
    fn paint(&self, text: &str) -> String {
        match self {
            Self::On => theme::success(text),
            Self::Off => theme::muted(text),
            Self::Number(_) => theme::title(text),
            Self::Text(_) => theme::muted(text),
            Self::Action => text.to_owned(),
        }
    }
}

pub fn heading(title: impl Into<String>) -> Row {
    Row::Heading(title.into())
}
pub fn item(label: impl Into<String>, value: Value) -> Row {
    Row::Item {
        label: label.into(),
        value,
    }
}

pub struct View<'a> {
    pub title: &'a str,
    pub subtitle: &'a str,
    /// Explains the highlighted row, or reports why the last action failed.
    pub note: &'a str,
    pub alert: bool,
    pub hint: &'a str,
    pub rows: &'a [Row],
    /// Index into `rows`; always an `Item`.
    pub selected: usize,
}

/// Repaint the whole menu, keeping the highlighted row inside the body with a
/// two-row margin. `scroll` is owned by the caller so resizing cannot jump.
pub fn draw(view: &View<'_>, scroll: &mut usize) -> io::Result<()> {
    let (columns, rows) = terminal::size()?;
    let width = (columns.clamp(24, 110) as usize).saturating_sub(4).max(16);
    let height = rows.max(6) as usize;
    let note = wrap(&safe(view.note), width);
    let note = &note[..note.len().min(2)];
    // Short menus hug the top rather than stretching the footer to the bottom.
    let body = height
        .saturating_sub(4 + note.len())
        .max(1)
        .min(view.rows.len().max(1));

    *scroll = clamp_scroll(*scroll, view.selected, view.rows.len(), body);

    let mut lines = Vec::with_capacity(height);
    let title = ellipsize(&safe(view.title), width);
    let subtitle = safe(view.subtitle);
    let gap = width.saturating_sub(title.width() + subtitle.width());
    lines.push(if gap > 0 && !subtitle.is_empty() {
        format!(
            "{}{}{}",
            theme::accent(&title),
            " ".repeat(gap),
            theme::muted(&subtitle)
        )
    } else {
        theme::accent(&title)
    });
    lines.push(theme::border(&"─".repeat(width)));
    for (index, row) in view.rows.iter().enumerate().skip(*scroll).take(body) {
        lines.push(paint_row(row, index == view.selected, width));
    }
    lines.push(theme::border(&"─".repeat(width)));
    for line in note {
        lines.push(if view.alert {
            theme::warning(line)
        } else {
            theme::muted(line)
        });
    }
    lines.push(paint_hint(&fit_hint(&safe(view.hint), width)));

    let mut out = io::stdout().lock();
    queue!(
        out,
        terminal::BeginSynchronizedUpdate,
        cursor::MoveTo(0, 0),
        Clear(ClearType::All)
    )?;
    for (row, line) in lines.iter().take(height).enumerate() {
        queue!(out, cursor::MoveTo(0, row as u16))?;
        write!(out, "  {line}")?;
    }
    queue!(out, terminal::EndSynchronizedUpdate)?;
    out.flush()
}

/// Rows the highlight keeps above and below itself while scrolling.
const MARGIN: usize = 2;

/// Keep `selected` inside the body with `MARGIN` rows of context on each side,
/// without scrolling past either end of the list.
fn clamp_scroll(scroll: usize, selected: usize, len: usize, body: usize) -> usize {
    let lowest = (selected + MARGIN + 1).saturating_sub(body);
    let highest = selected.saturating_sub(MARGIN).max(lowest);
    scroll.clamp(lowest, highest).min(len.saturating_sub(body))
}
/// Columns between the label and value columns.
const GAP: usize = 2;

fn paint_row(row: &Row, selected: bool, width: usize) -> String {
    let (label, value) = match row {
        Row::Heading(title) => {
            return theme::title(&ellipsize(&safe(title), width));
        }
        Row::Item { label, value } => (safe(label), value),
    };
    let text = safe(value.text());
    let label = ellipsize(&label, width.saturating_sub(text.width() + GAP + 2).max(1));
    let pad = width
        .saturating_sub(2 + label.width() + text.width())
        .max(GAP);
    if selected {
        theme::selected(&format!("› {label}{}{text}", " ".repeat(pad)))
    } else {
        format!(
            "  {label}{}{}",
            " ".repeat(pad),
            if text.is_empty() {
                String::new()
            } else {
                value.paint(&text)
            }
        )
    }
}

/// Drop middle hints rather than the tail when the terminal is narrow: the key
/// that leaves the menu is the one a reader most needs to still see.
fn fit_hint(hint: &str, width: usize) -> String {
    if hint.width() <= width {
        return hint.to_owned();
    }
    let parts: Vec<&str> = hint.split(" · ").collect();
    let Some((last, rest)) = parts.split_last() else {
        return clip(hint, width);
    };
    let mut kept = Vec::new();
    let mut cells = last.width();
    for part in rest {
        let next = cells + part.width() + 3;
        if next > width {
            break;
        }
        kept.push(*part);
        cells = next;
    }
    kept.push(last);
    clip(&kept.join(" · "), width)
}

fn paint_hint(hint: &str) -> String {
    hint.split(" · ")
        .map(|part| match part.split_once(' ') {
            Some((key, description)) => {
                format!("{} {}", theme::title(key), theme::muted(description))
            }
            None => theme::muted(part),
        })
        .collect::<Vec<_>>()
        .join(&theme::border(" · "))
}

/// Read one bounded line for the plain-terminal menus. An oversized line is
/// drained completely, so a rejected value can never reach the chat composer.
pub fn read_choice() -> io::Result<Option<String>> {
    read_choice_from(&mut io::stdin().lock())
}

pub(super) fn read_choice_from(input: &mut impl BufRead) -> io::Result<Option<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(row: &Row, selected: bool, width: usize) -> String {
        console::strip_ansi_codes(&paint_row(row, selected, width))
            .trim_end()
            .to_owned()
    }

    #[test]
    fn values_align_to_the_right_edge_and_selection_marks_the_row() {
        let row = item("Research pipeline", Value::On);
        assert_eq!(plain(&row, false, 30), "  Research pipeline         On");
        assert_eq!(plain(&row, true, 30), "› Research pipeline         On");
        assert_eq!(
            plain(
                &item("Agent rounds per run", Value::Number("100".into())),
                false,
                30
            ),
            "  Agent rounds per run     100"
        );
        assert_eq!(plain(&heading("Research"), false, 30), "Research");
    }

    #[test]
    fn narrow_rows_keep_the_value_visible_by_clipping_the_label() {
        let row = item("Semantic candidate pool", Value::Number("120".into()));
        let line = plain(&row, false, 16);
        assert!(line.ends_with("120"), "{line:?}");
        assert!(line.contains('…'), "{line:?}");
        assert!(console::strip_ansi_codes(&paint_row(&row, false, 16)).width() <= 16);
    }

    #[test]
    fn scroll_keeps_a_margin_around_the_selection_and_stops_at_the_ends() {
        let (len, body) = (20, 8);
        let mut scroll = clamp_scroll(0, 0, len, body);
        assert_eq!(scroll, 0);
        scroll = clamp_scroll(scroll, 5, len, body);
        assert_eq!(scroll, 0, "an in-view selection does not scroll");
        scroll = clamp_scroll(scroll, 6, len, body);
        assert_eq!(scroll, 1, "the bottom margin pushes the window down");
        scroll = clamp_scroll(scroll, 19, len, body);
        assert_eq!(scroll, 12, "the last row cannot scroll past the end");
        scroll = clamp_scroll(scroll, 13, len, body);
        assert_eq!(scroll, 11, "the top margin pulls the window back up");
        scroll = clamp_scroll(scroll, 0, len, body);
        assert_eq!(scroll, 0);
        assert_eq!(clamp_scroll(3, 4, 6, 8), 0, "a short list never scrolls");
    }

    #[test]
    fn a_narrow_hint_drops_middle_keys_but_keeps_the_way_out() {
        let hint = "↑↓ move · space toggle · / filter · esc cancel";
        assert_eq!(fit_hint(hint, 60), hint);
        assert_eq!(fit_hint(hint, 40), "↑↓ move · space toggle · esc cancel");
        assert_eq!(fit_hint(hint, 30), "↑↓ move · esc cancel");
        assert_eq!(fit_hint(hint, 10), "esc cancel");
        assert!(fit_hint(hint, 4).width() <= 4);
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
}

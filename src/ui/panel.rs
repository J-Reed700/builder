//! Aligned, sectioned panels for read-only reports such as `/help` and `/status`.
//! Layout is width-aware and stays readable without color, so `NO_COLOR` and
//! piped output keep the same columns.
use super::{safe, theme};
use crate::input::layout::{ellipsize, wrap};
use unicode_width::UnicodeWidthStr;

pub enum Row {
    Section(&'static str),
    Field { label: String, value: String },
    Note(String),
}

pub fn section(title: &'static str) -> Row {
    Row::Section(title)
}
pub fn field(label: impl Into<String>, value: impl Into<String>) -> Row {
    Row::Field {
        label: label.into(),
        value: value.into(),
    }
}
pub fn note(text: impl Into<String>) -> Row {
    Row::Note(text.into())
}

const MARGIN: &str = "  ";
const INDENT: &str = "    ";
/// Columns between the label and value columns.
const GAP: usize = 2;

/// Panels share the banner's width budget so a session reads as one surface.
pub fn width() -> usize {
    (console::Term::stdout().size().1 as usize).clamp(32, 96)
}

/// Group a count into thousands so long token and file totals stay readable.
pub fn count(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A proportion bar for a bounded resource, colored by how close it is to full.
pub fn meter(percent: usize, cells: usize) -> String {
    let filled = (percent.min(100) * cells).div_ceil(100).min(cells);
    let bar = "█".repeat(filled);
    let rest = "░".repeat(cells - filled);
    format!(
        "{}{}",
        match percent {
            0..=59 => theme::success(&bar),
            60..=84 => theme::warning(&bar),
            _ => theme::danger(&bar),
        },
        theme::border(&rest)
    )
}

/// Render a titled panel. The label column is sized from the longest label so
/// values line up, and long values wrap underneath their own column.
pub fn render(title: &str, subtitle: &str, rows: &[Row], width: usize) -> String {
    let inner = width.saturating_sub(MARGIN.len() * 2).max(16);
    let labels = rows
        .iter()
        .filter_map(|row| match row {
            Row::Field { label, .. } => Some(label.width()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let column = labels.min(inner / 2).max(1);
    let value_width = inner
        .saturating_sub(INDENT.len() - MARGIN.len() + column + GAP)
        .max(8);

    let mut out = String::new();
    let heading = safe(title);
    let trailing = safe(subtitle);
    let spacing = inner
        .saturating_sub(heading.width() + trailing.width())
        .max(1);
    out.push_str(MARGIN);
    out.push_str(&theme::accent(&heading));
    if !trailing.is_empty() {
        out.push_str(&" ".repeat(spacing));
        out.push_str(&theme::muted(&trailing));
    }
    out.push('\n');
    out.push_str(MARGIN);
    out.push_str(&theme::border(&"─".repeat(inner)));
    out.push('\n');

    for row in rows {
        match row {
            Row::Section(title) => {
                out.push('\n');
                out.push_str(MARGIN);
                out.push_str(&theme::title(&safe(title)));
                out.push('\n');
            }
            Row::Field { label, value } => {
                let label = ellipsize(&safe(label), column);
                let lines = wrap(&safe(value), value_width);
                for (index, line) in lines.iter().enumerate() {
                    let label = if index == 0 { label.as_str() } else { "" };
                    out.push_str(INDENT);
                    out.push_str(&theme::muted(label));
                    out.push_str(&" ".repeat(column - label.width() + GAP));
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Row::Note(text) => {
                for line in wrap(&safe(text), inner.saturating_sub(INDENT.len())) {
                    out.push_str(INDENT);
                    out.push_str(&theme::muted(&line));
                    out.push('\n');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Vec<String> {
        console::strip_ansi_codes(text)
            .lines()
            .map(|line| line.trim_end().to_owned())
            .collect()
    }

    #[test]
    fn fields_share_one_value_column_under_a_titled_rule() {
        let rows = [
            section("Conversation"),
            field("Messages", "128"),
            field("Estimated context", "42000 / 200000"),
        ];
        let lines = plain(&render("Session", "a1b2c3d4", &rows, 48));
        assert_eq!(lines[0], "  Session                             a1b2c3d4");
        assert_eq!(lines[1], format!("  {}", "─".repeat(44)));
        assert_eq!(lines[3], "  Conversation");
        assert_eq!(lines[4], "    Messages           128");
        assert_eq!(lines[5], "    Estimated context  42000 / 200000");
    }

    #[test]
    fn long_values_wrap_under_their_column_and_control_bytes_are_removed() {
        let rows = [field(
            "Code index",
            "generation 4 \x1b[31mwith many files and chunks recorded here",
        )];
        let lines = plain(&render("Status", "", &rows, 40));
        assert!(lines.iter().all(|line| !line.contains("[31m")), "{lines:?}");
        assert!(
            lines[2].starts_with("    Code index  generation"),
            "{lines:?}"
        );
        assert_eq!(lines[3], "                files and chunks", "{lines:?}");
    }

    #[test]
    fn counts_are_grouped_into_thousands() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(200_000), "200,000");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn meter_fills_proportionally_and_never_overflows() {
        assert_eq!(
            console::strip_ansi_codes(&meter(0, 10)).to_string(),
            "░".repeat(10)
        );
        assert_eq!(
            console::strip_ansi_codes(&meter(100, 10)).to_string(),
            "█".repeat(10)
        );
        assert_eq!(
            console::strip_ansi_codes(&meter(240, 4)).to_string(),
            "█".repeat(4)
        );
        assert_eq!(
            console::strip_ansi_codes(&meter(45, 10)).to_string(),
            format!("{}{}", "█".repeat(5), "░".repeat(5))
        );
    }
}

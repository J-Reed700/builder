use super::buffer::Buffer;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Visual cursor positions for every atom boundary; pasted contents are never
/// expanded here. Layout depends on visible draft size, not clipboard size.
pub struct Layout {
    pub lines: Vec<String>,
    pub positions: Vec<(usize, usize)>,
    pub cursor: (usize, usize),
}
impl Layout {
    pub fn new(buffer: &Buffer, width: usize) -> Self {
        let width = width.max(1);
        let mut lines = vec![String::new()];
        let mut positions = vec![];
        let mut row = 0;
        let mut col = 0;
        for atom in &buffer.draft.atoms {
            positions.push((row, col));
            for grapheme in atom.display().graphemes(true) {
                if matches!(grapheme, "\n" | "\r\n" | "\r") {
                    lines.push(String::new());
                    row += 1;
                    col = 0;
                    continue;
                }
                let display = if grapheme == "\t" { "    " } else { grapheme };
                let clean: String = display.chars().filter(|c| !c.is_control()).collect();
                let cells = clean.width();
                if col + cells > width && col > 0 {
                    lines.push(String::new());
                    row += 1;
                    col = 0;
                }
                lines[row].push_str(&clean);
                col += cells;
                if col >= width {
                    lines.push(String::new());
                    row += 1;
                    col = 0;
                }
            }
        }
        positions.push((row, col));
        let cursor = positions[buffer.draft.cursor];
        Self {
            lines,
            positions,
            cursor,
        }
    }
    pub fn vertical(&self, cursor: usize, down: bool) -> usize {
        let (row, col) = self.positions[cursor];
        let target = if down { row + 1 } else { row.saturating_sub(1) };
        self.positions
            .iter()
            .enumerate()
            .filter(|(_, p)| p.0 == target)
            .min_by_key(|(_, p)| p.1.abs_diff(col))
            .map_or(cursor, |(index, _)| index)
    }
}

pub fn clip(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut cells = 0;
    for grapheme in text.graphemes(true) {
        let next = grapheme.width();
        if cells + next > width {
            break;
        }
        out.push_str(grapheme);
        cells += next;
    }
    out
}

/// Wrap transcript text at word boundaries while preserving explicit lines.
/// The composer itself still uses `Layout`, whose atom positions must remain
/// exact for cursor movement; this is only for the read-only submitted copy.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut cells = 0;

    for grapheme in text.graphemes(true) {
        if matches!(grapheme, "\n" | "\r\n" | "\r") {
            lines.push(line.trim_end().to_owned());
            line.clear();
            cells = 0;
            continue;
        }
        let display = if grapheme == "\t" { "    " } else { grapheme };
        let clean: String = display.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            continue;
        }
        let next = clean.width();

        if cells + next > width && cells > 0 {
            if clean.chars().all(char::is_whitespace) {
                lines.push(line.trim_end().to_owned());
                line.clear();
                cells = 0;
                continue;
            }

            if line.ends_with(char::is_whitespace) {
                lines.push(line.trim_end().to_owned());
                line.clear();
                cells = 0;
            } else if let Some(at) = word_break(&line) {
                let tail = line[at..].trim_start().to_owned();
                line.truncate(at);
                lines.push(line.trim_end().to_owned());
                line = tail;
                cells = line.width();
            } else {
                lines.push(std::mem::take(&mut line));
                cells = 0;
            }

            if cells + next > width && cells > 0 {
                lines.push(std::mem::take(&mut line));
                cells = 0;
            }
        }

        line.push_str(&clean);
        cells += next;
    }
    lines.push(line.trim_end().to_owned());
    lines
}

fn word_break(line: &str) -> Option<usize> {
    line.grapheme_indices(true)
        .rev()
        .find_map(|(at, grapheme)| {
            (grapheme.chars().all(char::is_whitespace)
                && !line[..at].trim().is_empty()
                && !line[at + grapheme.len()..].trim().is_empty())
            .then_some(at)
        })
}

/// Clip a single-line label and mark that text was omitted.
pub fn ellipsize(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    match width {
        0 => String::new(),
        1 => "…".into(),
        width => format!("{}…", clip(text, width - 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_wrap_keeps_words_and_explicit_lines_intact() {
        assert_eq!(
            wrap("I need a full offline copy of my courses", 16),
            ["I need a full", "offline copy of", "my courses"]
        );
        assert_eq!(wrap("one\ntwo", 20), ["one", "two"]);
        assert_eq!(wrap("extraordinary", 5), ["extra", "ordin", "ary"]);
    }

    #[test]
    fn transcript_wrap_uses_display_cells_and_ellipsizes() {
        assert_eq!(wrap("ab 🦀 cd", 5), ["ab 🦀", "cd"]);
        assert_eq!(ellipsize("Cargo.toml", 7), "Cargo.…");
        assert_eq!(ellipsize("ok", 7), "ok");
    }
}

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

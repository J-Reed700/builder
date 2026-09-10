//! Stateful escape filtering and a bounded coalescing buffer. This is independent
//! of the terminal, so output integrity can be tested at arbitrary chunk splits.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
#[derive(Default)]
enum Escape {
    #[default]
    Text,
    Start,
    Csi,
    Osc,
    OscEnd,
}

#[derive(Default)]
pub struct Sanitizer {
    state: Escape,
}
impl Sanitizer {
    pub fn push(&mut self, input: &str, output: &mut String) {
        for c in input.chars() {
            match self.state {
                Escape::Text => match c {
                    '\x1b' => self.state = Escape::Start,
                    '\u{009b}' => self.state = Escape::Csi,
                    '\u{009d}' => self.state = Escape::Osc,
                    '\n' | '\t' => output.push(c),
                    c if !c.is_control() => output.push(c),
                    _ => {}
                },
                Escape::Start => {
                    self.state = match c {
                        '[' => Escape::Csi,
                        ']' | 'P' | '^' | '_' => Escape::Osc,
                        _ => Escape::Text,
                    }
                }
                Escape::Csi => {
                    if ('@'..='~').contains(&c) {
                        self.state = Escape::Text;
                    }
                }
                Escape::Osc => match c {
                    '\x07' | '\u{009c}' => self.state = Escape::Text,
                    '\x1b' => self.state = Escape::OscEnd,
                    _ => {}
                },
                Escape::OscEnd => self.state = if c == '\\' { Escape::Text } else { Escape::Osc },
            }
        }
    }
}

#[derive(Default)]
pub struct StreamBuffer {
    pending: String,
    sanitizer: Sanitizer,
}
impl StreamBuffer {
    pub fn push(&mut self, text: &str) {
        self.sanitizer.push(text, &mut self.pending);
    }
    pub fn len(&self) -> usize {
        self.pending.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
    pub fn reset(&mut self) {
        self.pending.clear();
        self.sanitizer = Sanitizer::default();
    }
}

/// Incremental word-aware wrapping for the human transcript. A partial word is
/// held until its next separator, so arbitrary provider chunk boundaries cannot
/// split it across display lines. Headless output remains byte-for-byte content.
pub struct Reflow {
    word: String,
    separator: String,
    column: usize,
    line_start: bool,
}

impl Default for Reflow {
    fn default() -> Self {
        Self {
            word: String::new(),
            separator: String::new(),
            column: 0,
            line_start: true,
        }
    }
}

impl Reflow {
    pub fn push(&mut self, input: &str, width: usize, indent: &str) -> String {
        let width = width.max(1);
        let mut out = String::new();
        for grapheme in input.graphemes(true) {
            if matches!(grapheme, "\n" | "\r\n" | "\r") {
                self.emit_word(&mut out, width, indent);
                self.separator.clear();
                out.push('\n');
                self.column = 0;
                self.line_start = true;
            } else if grapheme.chars().all(char::is_whitespace) {
                self.emit_word(&mut out, width, indent);
                if grapheme == "\t" {
                    self.separator.push_str("    ");
                } else {
                    self.separator.push_str(grapheme);
                }
            } else {
                self.word.push_str(grapheme);
                // Bound an endpoint-controlled token even if it never emits a
                // separator (a URL, minified source, or malformed response).
                if self.word.width() >= width {
                    self.emit_word(&mut out, width, indent);
                }
            }
        }
        out
    }

    pub fn finish(&mut self, width: usize, indent: &str) -> String {
        let mut out = String::new();
        self.emit_word(&mut out, width.max(1), indent);
        self.separator.clear();
        out
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn emit_word(&mut self, out: &mut String, width: usize, indent: &str) {
        if self.word.is_empty() {
            return;
        }
        if !self.line_start && self.column + self.separator.width() + self.word.width() > width {
            out.push('\n');
            self.column = 0;
            self.line_start = true;
            self.separator.clear();
        }
        let separator = std::mem::take(&mut self.separator);
        let word = std::mem::take(&mut self.word);
        self.emit_text(out, &separator, width, indent);
        self.emit_text(out, &word, width, indent);
    }

    fn emit_text(&mut self, out: &mut String, text: &str, width: usize, indent: &str) {
        for grapheme in text.graphemes(true) {
            let cells = grapheme.width();
            if self.column + cells > width && self.column > 0 {
                out.push('\n');
                self.column = 0;
                self.line_start = true;
            }
            if self.line_start {
                out.push_str(indent);
                self.line_start = false;
            }
            out.push_str(grapheme);
            self.column += cells;
        }
    }
}

#[cfg(test)]
mod reflow_tests {
    use super::Reflow;

    #[test]
    fn streamed_prose_wraps_words_independent_of_chunk_boundaries() {
        let mut whole = Reflow::default();
        let mut expected = whole.push("A complete sentence wraps cleanly", 16, "    ");
        expected.push_str(&whole.finish(16, "    "));

        let mut split = Reflow::default();
        let mut actual = split.push("A comp", 16, "    ");
        actual.push_str(&split.push("lete sentence wr", 16, "    "));
        actual.push_str(&split.push("aps cleanly", 16, "    "));
        actual.push_str(&split.finish(16, "    "));

        assert_eq!(actual, expected);
        assert_eq!(actual, "    A complete\n    sentence wraps\n    cleanly");
    }

    #[test]
    fn streamed_prose_preserves_explicit_lines_and_bounds_long_tokens() {
        let mut reflow = Reflow::default();
        let mut text = reflow.push("one\n\n  two abcdefghi", 6, "  ");
        text.push_str(&reflow.finish(6, "  "));
        assert_eq!(text, "  one\n\n    two\n  abcdef\n  ghi");
    }
}

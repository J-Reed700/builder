//! Stateful escape filtering and a bounded coalescing buffer. This is independent
//! of the terminal, so output integrity can be tested at arbitrary chunk splits.
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

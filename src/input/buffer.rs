//! The composer stores large pastes as shared immutable blocks. Editing and
//! repainting the surrounding prompt never scans or copies their contents.
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_UNDO: usize = 100;
const MAX_UNDO_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum Atom {
    Text(String),
    Paste { text: Arc<str>, label: String },
}
impl Atom {
    pub fn display(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Paste { label, .. } => label,
        }
    }
    fn content(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Paste { text, .. } => text,
        }
    }
}

#[derive(Clone, Default)]
pub struct Draft {
    pub atoms: Vec<Atom>,
    pub cursor: usize,
    pub bytes: usize,
}

#[derive(Default)]
pub struct Buffer {
    pub draft: Draft,
    undo: Vec<Change>,
    redo: Vec<Change>,
    next_paste: usize,
    undo_bytes: usize,
}
#[derive(Clone)]
struct Change {
    at: usize,
    removed: Vec<Atom>,
    inserted: Vec<Atom>,
    before: usize,
    after: usize,
}

impl Buffer {
    pub fn is_empty(&self) -> bool {
        self.draft.atoms.is_empty()
    }
    pub fn content(&self) -> String {
        let mut text = String::with_capacity(self.draft.bytes);
        for atom in &self.draft.atoms {
            text.push_str(atom.content());
        }
        text
    }
    pub fn restore(&mut self, draft: Draft) {
        self.draft = draft;
        self.undo.clear();
        self.redo.clear();
        self.undo_bytes = 0;
    }
    pub fn insert(&mut self, text: &str, paste: bool) -> Result<(), &'static str> {
        if self.draft.bytes.saturating_add(text.len()) > MAX_INPUT_BYTES {
            return Err("Draft limit is 4 MiB. Nothing was inserted; attach a file instead.");
        }
        if text.is_empty() {
            return Ok(());
        }
        if !paste
            && self.draft.cursor > 0
            && text.graphemes(true).count() == 1
            && let Atom::Text(previous) = &self.draft.atoms[self.draft.cursor - 1]
        {
            let joined = format!("{previous}{text}");
            if joined.graphemes(true).count() == 1 {
                self.change(self.draft.cursor - 1, 1, vec![Atom::Text(joined)]);
                return Ok(());
            }
        }
        let atoms =
            if paste && (text.len() >= 256 || text.bytes().filter(|b| *b == b'\n').count() >= 2) {
                self.next_paste += 1;
                let lines = text.bytes().filter(|b| *b == b'\n').count() + 1;
                vec![Atom::Paste {
                    text: Arc::from(text),
                    label: format!(
                        "[paste {} · {lines} lines · {}]",
                        self.next_paste,
                        size(text.len())
                    ),
                }]
            } else {
                text.graphemes(true)
                    .map(|s| Atom::Text(s.to_owned()))
                    .collect()
            };
        let at = self.draft.cursor;
        self.change(at, 0, atoms);
        Ok(())
    }
    pub fn backspace(&mut self) {
        if self.draft.cursor > 0 {
            self.change(self.draft.cursor - 1, 1, vec![]);
        }
    }
    pub fn delete(&mut self) {
        if self.draft.cursor < self.draft.atoms.len() {
            self.change(self.draft.cursor, 1, vec![]);
        }
    }
    pub fn clear(&mut self) {
        self.change(0, self.draft.atoms.len(), vec![]);
    }
    pub fn left(&mut self) {
        self.draft.cursor = self.draft.cursor.saturating_sub(1);
    }
    pub fn right(&mut self) {
        self.draft.cursor = (self.draft.cursor + 1).min(self.draft.atoms.len());
    }
    pub fn home(&mut self) {
        self.draft.cursor = self.draft.atoms[..self.draft.cursor]
            .iter()
            .rposition(|a| a.display() == "\n")
            .map_or(0, |i| i + 1);
    }
    pub fn end(&mut self) {
        self.draft.cursor = self.draft.atoms[self.draft.cursor..]
            .iter()
            .position(|a| a.display() == "\n")
            .map_or(self.draft.atoms.len(), |i| i + self.draft.cursor);
    }
    pub fn delete_word(&mut self) {
        let end = self.draft.cursor;
        let mut start = end;
        while start > 0 && self.draft.atoms[start - 1].display().trim().is_empty() {
            start -= 1;
        }
        while start > 0 && !self.draft.atoms[start - 1].display().trim().is_empty() {
            start -= 1;
        }
        if end > start {
            self.change(start, end - start, vec![]);
        }
    }
    pub fn undo(&mut self) {
        if let Some(change) = self.undo.pop() {
            self.undo_bytes -= change.bytes();
            self.replace(change.at, change.inserted.len(), &change.removed);
            self.draft.cursor = change.before;
            self.redo.push(change);
        }
    }
    pub fn redo(&mut self) {
        if let Some(change) = self.redo.pop() {
            self.undo_bytes += change.bytes();
            self.replace(change.at, change.removed.len(), &change.inserted);
            self.draft.cursor = change.after;
            self.undo.push(change);
        }
    }
    pub fn pastes(&self) -> usize {
        self.draft
            .atoms
            .iter()
            .filter(|a| matches!(a, Atom::Paste { .. }))
            .count()
    }
    fn change(&mut self, at: usize, count: usize, inserted: Vec<Atom>) {
        let removed = self.draft.atoms[at..at + count].to_vec();
        let change = Change {
            at,
            removed,
            after: at + inserted.len(),
            inserted,
            before: self.draft.cursor,
        };
        self.replace(at, count, &change.inserted);
        self.draft.cursor = change.after;
        self.undo_bytes += change.bytes();
        self.undo.push(change);
        while self.undo.len() > MAX_UNDO || self.undo_bytes > MAX_UNDO_BYTES {
            self.undo_bytes -= self.undo.remove(0).bytes();
        }
        self.redo.clear();
    }
    fn replace(&mut self, at: usize, count: usize, inserted: &[Atom]) {
        self.draft.bytes -= self.draft.atoms[at..at + count]
            .iter()
            .map(|a| a.content().len())
            .sum::<usize>();
        self.draft.bytes += inserted.iter().map(|a| a.content().len()).sum::<usize>();
        self.draft
            .atoms
            .splice(at..at + count, inserted.iter().cloned());
    }
}

impl Change {
    fn bytes(&self) -> usize {
        self.removed
            .iter()
            .chain(&self.inserted)
            .map(|a| a.content().len())
            .sum()
    }
}

pub fn size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

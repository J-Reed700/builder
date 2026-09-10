use builder::input::{
    buffer::{Atom, Buffer, MAX_INPUT_BYTES},
    layout::Layout,
};
use builder::ui::stream::{Sanitizer, StreamBuffer};
use std::sync::Arc;

#[test]
fn pasted_source_and_paths_are_not_treated_as_slash_commands() {
    for text in [
        "// a code comment",
        "/** docs */",
        "/src/main.rs",
        "// header\nfn main() {}",
    ] {
        assert!(!builder::input::is_unknown_command(text));
    }
    assert!(builder::input::is_unknown_command("/unknown"));
}

#[test]
fn megabyte_paste_is_one_shared_atom_and_expands_losslessly() {
    let text = "  fn main() { /* 🦀 */ }\r\n".repeat(40000);
    let mut buffer = Buffer::default();
    buffer.insert("Review:\n", false).unwrap();
    let before = buffer.draft.atoms.len();
    buffer.insert(&text, true).unwrap();
    assert_eq!(buffer.draft.atoms.len(), before + 1);
    let snapshot = buffer.draft.clone();
    if let (Atom::Paste { text: a, .. }, Atom::Paste { text: b, .. }) =
        (&buffer.draft.atoms[before], &snapshot.atoms[before])
    {
        assert!(Arc::ptr_eq(a, b));
    } else {
        panic!("paste was expanded into individual characters");
    }
    buffer.insert("\nPlease verify.", false).unwrap();
    assert_eq!(buffer.content(), format!("Review:\n{text}\nPlease verify."));
    let layout = Layout::new(&buffer, 80);
    assert!(layout.lines.iter().map(String::len).sum::<usize>() < 200);
}

#[test]
fn undo_and_redo_a_large_paste_and_surrounding_edits() {
    let mut buffer = Buffer::default();
    let paste = "source\n".repeat(10000);
    buffer.insert("before ", false).unwrap();
    buffer.insert(&paste, true).unwrap();
    buffer.insert(" after", false).unwrap();
    buffer.undo();
    buffer.undo();
    assert_eq!(buffer.content(), "before ");
    buffer.redo();
    buffer.redo();
    assert_eq!(buffer.content(), format!("before {paste} after"));
    buffer.clear();
    assert!(buffer.is_empty());
    buffer.undo();
    assert_eq!(buffer.content(), format!("before {paste} after"));
}

#[test]
fn backspace_removes_a_paste_as_a_whole_and_undo_restores_it() {
    let mut buffer = Buffer::default();
    let paste = "x".repeat(1024);
    buffer.insert(&paste, true).unwrap();
    buffer.backspace();
    assert_eq!(buffer.draft.bytes, 0);
    buffer.undo();
    assert_eq!(buffer.content(), paste);
}

#[test]
fn input_limit_rejects_whole_paste_without_mutating_draft() {
    let mut buffer = Buffer::default();
    buffer.insert("keep this", false).unwrap();
    assert!(buffer.insert(&"x".repeat(MAX_INPUT_BYTES), true).is_err());
    assert_eq!(buffer.content(), "keep this");
}

#[test]
fn unicode_editing_keeps_graphemes_intact() {
    let mut buffer = Buffer::default();
    buffer.insert("a👩‍💻界e\u{301}", true).unwrap();
    buffer.backspace();
    assert_eq!(buffer.content(), "a👩‍💻界");
    buffer.left();
    buffer.backspace();
    assert_eq!(buffer.content(), "a界");
    buffer.insert("e", false).unwrap();
    buffer.insert("\u{301}", false).unwrap();
    buffer.backspace();
    assert_eq!(buffer.content(), "a界");
}

#[test]
fn cursor_wraps_using_terminal_cell_width() {
    let mut buffer = Buffer::default();
    buffer.insert("ab界cd\nxyz", false).unwrap();
    let layout = Layout::new(&buffer, 4);
    assert_eq!(layout.lines, vec!["ab界", "cd", "xyz"]);
    assert_eq!(layout.cursor, (2, 3));
    let up = layout.vertical(buffer.draft.cursor, false);
    assert_eq!(layout.positions[up], (1, 2));
}

#[test]
fn stream_buffer_coalesces_deltas_without_changing_text() {
    let mut stream = StreamBuffer::default();
    for part in ["hello", " ", "🦀", "\n", "  code"] {
        stream.push(part);
    }
    assert_eq!(stream.take(), "hello 🦀\n  code");
    assert!(stream.is_empty());
}

#[test]
fn escape_sequences_are_removed_even_across_chunk_boundaries() {
    let text = "hi\x1b[31m red\x1b[0m\x1b]52;c;clipboard\x07!";
    for split in 0..=text.len() {
        let mut filter = Sanitizer::default();
        let mut out = String::new();
        filter.push(&text[..split], &mut out);
        filter.push(&text[split..], &mut out);
        assert_eq!(out, "hi red!");
    }
}

#[test]
fn retry_resets_stream_escape_state_and_provisional_buffer() {
    let mut stream = StreamBuffer::default();
    stream.push("old response\x1b]");
    stream.reset();
    stream.push("fresh response");
    assert_eq!(stream.take(), "fresh response");
}

#[test]
fn home_end_and_word_deletion_work_on_multiline_text() {
    let mut buffer = Buffer::default();
    buffer.insert("first\nsecond third", false).unwrap();
    buffer.delete_word();
    assert_eq!(buffer.content(), "first\nsecond ");
    buffer.home();
    buffer.insert("> ", false).unwrap();
    buffer.end();
    buffer.insert("last", false).unwrap();
    assert_eq!(buffer.content(), "first\n> second last");
}

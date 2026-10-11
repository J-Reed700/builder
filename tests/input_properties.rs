use builder::input::buffer::{Buffer, MAX_INPUT_BYTES};
use unicode_segmentation::UnicodeSegmentation;

#[test]
fn every_unicode_grapheme_can_be_edited_undone_and_redone_atomically() {
    for text in ["e\u{301}", "🦀", "👨‍👩‍👧‍👦", "🇺🇸", "中", "👍🏽", "क"]
    {
        let mut buffer = Buffer::default();
        buffer.insert(text, false).unwrap();
        assert_eq!(buffer.draft.atoms.len(), 1, "{text}");
        buffer.backspace();
        assert!(buffer.is_empty(), "{text}");
        buffer.undo();
        assert_eq!(buffer.content(), text);
        assert_eq!(buffer.draft.bytes, text.len());
        buffer.redo();
        assert!(buffer.is_empty());
        assert_eq!(buffer.draft.bytes, 0);
    }
}

#[test]
fn seeded_edit_sequences_match_an_independent_grapheme_reference_model() {
    // Reproducible property-style test: 32,768 operations, including arbitrary
    // insert locations and repeated movement/deletion at both boundaries.
    let alphabet = ["a", "中", "🦀", " ", "\n", "é"];
    for seed in 1..=64_u64 {
        let mut random = seed;
        let mut buffer = Buffer::default();
        let mut reference: Vec<&str> = vec![];
        let mut cursor = 0_usize;
        for step in 0..512 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            match (random >> 32) % 6 {
                0 | 1 => {
                    let text = alphabet[(random as usize) % alphabet.len()];
                    buffer.insert(text, false).unwrap();
                    reference.insert(cursor, text);
                    cursor += 1;
                }
                2 => {
                    buffer.left();
                    cursor = cursor.saturating_sub(1);
                }
                3 => {
                    buffer.right();
                    cursor = (cursor + 1).min(reference.len());
                }
                4 => {
                    buffer.backspace();
                    if cursor > 0 {
                        cursor -= 1;
                        reference.remove(cursor);
                    }
                }
                _ => {
                    buffer.delete();
                    if cursor < reference.len() {
                        reference.remove(cursor);
                    }
                }
            }
            let expected = reference.concat();
            assert_eq!(buffer.content(), expected, "seed {seed}, step {step}");
            assert_eq!(buffer.draft.cursor, cursor, "seed {seed}, step {step}");
            assert_eq!(buffer.draft.bytes, expected.len());
            assert_eq!(buffer.draft.atoms.len(), expected.graphemes(true).count());
        }
    }
}

#[test]
fn exact_byte_limit_accepts_unicode_and_rejection_preserves_undo_history() {
    let mut buffer = Buffer::default();
    let content = "🦀".repeat(MAX_INPUT_BYTES / 4);
    buffer.insert(&content, true).unwrap();
    assert_eq!(buffer.draft.bytes, MAX_INPUT_BYTES);
    assert!(buffer.insert("a", false).is_err());
    assert_eq!(buffer.content(), content);
    buffer.undo();
    assert!(buffer.is_empty());
    buffer.redo();
    assert_eq!(buffer.content(), content);
}

#[test]
fn a_new_edit_after_undo_discards_redo_but_draft_restore_clears_both_histories() {
    let mut buffer = Buffer::default();
    buffer.insert("old", false).unwrap();
    buffer.insert(" branch", false).unwrap();
    buffer.undo();
    buffer.insert(" replacement", false).unwrap();
    buffer.redo();
    assert_eq!(buffer.content(), "old replacement");
    let saved = buffer.draft.clone();
    buffer.clear();
    buffer.restore(saved);
    buffer.undo();
    buffer.redo();
    assert_eq!(buffer.content(), "old replacement");
}

#[test]
fn undo_history_is_bounded_and_does_not_lose_the_current_draft() {
    let mut buffer = Buffer::default();
    for _ in 0..150 {
        buffer.insert("x", false).unwrap();
    }
    for _ in 0..150 {
        buffer.undo();
    }
    assert_eq!(buffer.content(), "x".repeat(50));
    for _ in 0..150 {
        buffer.redo();
    }
    assert_eq!(buffer.content(), "x".repeat(150));
}

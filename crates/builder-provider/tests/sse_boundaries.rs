use builder_provider::SseDecoder;

#[test]
fn every_two_and_three_chunk_partition_preserves_unicode_and_event_boundaries() {
    let input =
        "data: 🦀 café\r\ndata: 中文\r\n\r\n:heartbeat\ndata:\n\ndata: [DONE]\n\n".as_bytes();
    let expected = vec!["🦀 café\n中文", "", "[DONE]"];
    for first in 0..=input.len() {
        for second in first..=input.len() {
            let mut decoder = SseDecoder::default();
            let mut actual = vec![];
            for part in [&input[..first], &input[first..second], &input[second..]] {
                actual.extend(decoder.push(part).unwrap());
            }
            assert_eq!(actual, expected, "split at {first}, {second}");
        }
    }
}

#[test]
fn byte_at_a_time_delivery_preserves_combining_marks_and_emoji_sequences() {
    let expected = "e\u{301} 👨‍👩‍👧‍👦 🇺🇸\t<tool>";
    let mut decoder = SseDecoder::default();
    let mut events = vec![];
    for byte in format!("data: {expected}\n\n").as_bytes() {
        events.extend(decoder.push(&[*byte]).unwrap());
    }
    assert_eq!(events, [expected]);
}

#[test]
fn comments_unknown_fields_and_empty_pushes_do_not_emit_events() {
    let mut decoder = SseDecoder::default();
    for bytes in [
        b"".as_slice(),
        b": heartbeat\n\n",
        b"event: message\nid: 1\nretry: 1000\n\n",
    ] {
        assert!(decoder.push(bytes).unwrap().is_empty());
    }
    assert_eq!(
        decoder.push(b"data:  keep one space\n\n").unwrap(),
        [" keep one space"]
    );
}

#[test]
fn incomplete_lines_and_events_stay_provisional() {
    let mut decoder = SseDecoder::default();
    assert!(
        decoder
            .push(b"data: never dispatch partial")
            .unwrap()
            .is_empty()
    );
    assert!(decoder.push(b"\n").unwrap().is_empty());
    assert_eq!(decoder.push(b"\n").unwrap(), ["never dispatch partial"]);
    assert!(decoder.push(b"\n\n").unwrap().is_empty());
}

#[test]
fn invalid_utf8_is_rejected_only_after_the_line_is_complete() {
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b"data: \xf0\x9f").unwrap().is_empty());
    assert!(decoder.push(b"\n\n").is_err());
}

#[test]
fn oversized_unterminated_lines_are_rejected() {
    let mut decoder = SseDecoder::default();
    assert!(
        decoder
            .push(&vec![b'x'; 4 * 1024 * 1024])
            .unwrap()
            .is_empty()
    );
    assert!(
        decoder
            .push(b"x")
            .unwrap_err()
            .to_string()
            .contains("SSE buffer")
    );
}

#[test]
fn multiline_event_limit_counts_the_newline_separators_too() {
    let mut decoder = SseDecoder::default();
    let payload = "x".repeat(4 * 1024 * 1024 - 16);
    assert!(
        decoder
            .push(format!("data:{payload}\n").as_bytes())
            .unwrap()
            .is_empty()
    );
    for _ in 0..16 {
        assert!(decoder.push(b"data:\n").unwrap().is_empty());
    }
    assert!(
        decoder
            .push(b"data:\n")
            .unwrap_err()
            .to_string()
            .contains("SSE event")
    );
}

#[test]
fn event_budget_resets_after_each_committed_event() {
    let mut decoder = SseDecoder::default();
    let data = "x".repeat(1024 * 1024);
    for _ in 0..8 {
        assert_eq!(
            decoder
                .push(format!("data: {data}\n\n").as_bytes())
                .unwrap(),
            [data.as_str()]
        );
    }
}

use builder_core::protocol::{IMAGE_PROMPT_BYTES, Image, Message, Role};

#[test]
fn wire_content_is_plain_text_without_images() {
    let message = Message::text(Role::User, "hello");
    assert_eq!(message.wire_content(), serde_json::json!("hello"));
    let empty = Message::text(Role::User, "");
    assert_eq!(empty.wire_content(), serde_json::json!(""));
}

#[test]
fn wire_content_is_a_multimodal_array_with_images() {
    let message = Message::text(Role::User, "what is this?").with_images(vec![Image {
        mime: "image/png".into(),
        data: "aGVsbG8=".into(),
    }]);
    assert_eq!(
        message.wire_content(),
        serde_json::json!([
            { "type": "text", "text": "what is this?" },
            {
                "type": "image_url",
                "image_url": { "url": "data:image/png;base64,aGVsbG8=" }
            }
        ])
    );
}

#[test]
fn image_only_message_omits_the_text_part() {
    let message = Message::text(Role::User, "").with_images(vec![Image {
        mime: "image/png".into(),
        data: "AA==".into(),
    }]);
    let wire = message.wire_content();
    let parts = wire.as_array().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["type"], "image_url");
}

#[test]
fn prompt_bytes_charge_each_image_a_fixed_budget_not_its_payload() {
    let plain = Message::text(Role::User, "hello");
    let image = |len: usize| Image {
        mime: "image/png".into(),
        data: "x".repeat(len),
    };
    let small = Message::text(Role::User, "hello").with_images(vec![image(100)]);
    // A multi-megabyte screenshot must not read as a million tokens.
    let large = Message::text(Role::User, "hello").with_images(vec![image(3_000_000)]);
    let base = plain.prompt_bytes().unwrap();
    assert_eq!(small.prompt_bytes().unwrap(), base + IMAGE_PROMPT_BYTES);
    assert_eq!(large.prompt_bytes().unwrap(), base + IMAGE_PROMPT_BYTES);
    // Counting must agree with an actual serialization of the text form.
    assert_eq!(
        base,
        serde_json::to_vec(&serde_json::json!({"role":"user","content":"hello"}))
            .unwrap()
            .len()
    );
}

#[test]
fn text_with_image_notes_replaces_payloads_with_a_marker() {
    let image = Image {
        mime: "image/png".into(),
        data: "AA==".into(),
    };
    assert_eq!(
        Message::text(Role::User, "plain")
            .text_with_image_notes()
            .as_deref(),
        Some("plain")
    );
    let one = Message::text(Role::User, "look").with_images(vec![image.clone()]);
    assert_eq!(
        one.text_with_image_notes().as_deref(),
        Some("look\n[1 image attached; not shown here]")
    );
    let only = Message::text(Role::User, "").with_images(vec![image.clone(), image]);
    assert_eq!(
        only.text_with_image_notes().as_deref(),
        Some("[2 images attached; not shown here]")
    );
}

#[test]
fn images_survive_a_json_roundtrip_and_stay_out_of_plain_transcripts() {
    let image = Image {
        mime: "image/png".into(),
        data: "aGVsbG8=".into(),
    };
    let message = Message::text(Role::User, "look").with_images(vec![image.clone()]);
    let json = serde_json::to_string(&message).unwrap();
    assert!(json.contains("aGVsbG8="));
    assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
    // A message without images never serializes the field, so old
    // transcripts stay unchanged and still deserialize.
    let plain = serde_json::to_string(&Message::text(Role::User, "plain")).unwrap();
    assert!(!plain.contains("images"));
    let legacy: Message = serde_json::from_str(r#"{"role":"user","content":"legacy"}"#).unwrap();
    assert!(legacy.images.is_empty());
}

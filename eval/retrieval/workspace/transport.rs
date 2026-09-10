fn legacy_serializer() {}

/// Keep private reasoning out of serialized conversation payloads.
fn serialize_conversation() {
    struct Payload { text: String }
    encode(Payload { text: visible_text() });
}

fn transport_statistics() { count_requests(); }

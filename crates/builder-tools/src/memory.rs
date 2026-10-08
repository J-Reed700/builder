//! Schemas for application-owned memory operations.
use crate::schema;
use serde_json::{Value, json};

pub fn definitions() -> Vec<Value> {
    vec![
        schema(
            "memory_search",
            "Retrieve source-backed findings for this checkout. Stale results are leads only.",
            json!({"query":{"type":"string"}}),
            &["query"],
        ),
        schema(
            "memory_get",
            "Inspect a memory and its evidence/revision; historical versions may be stale.",
            json!({"key":{"type":"string"},"revision":{"type":"integer"}}),
            &["key"],
        ),
        schema(
            "memory_upsert",
            "Store/update one reusable repository finding supported by recent successful read_file IDs. Use expected_revision=0 to create, otherwise current revision. Never store permissions or plans as facts. Does not count as task progress.",
            json!({"key":{"type":"string"},"text":{"type":"string"},"expected_revision":{"type":"integer","minimum":0},"evidence_call_ids":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":8}}),
            &["key", "text", "expected_revision", "evidence_call_ids"],
        ),
        schema(
            "memory_forget",
            "Remove a checkout finding from retrieval, retaining audit history. Requires approval.",
            json!({"key":{"type":"string"},"expected_revision":{"type":"integer"}}),
            &["key", "expected_revision"],
        ),
    ]
}

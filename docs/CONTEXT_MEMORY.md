# Source-backed context memory

Builder compaction has four layers:

1. Original transcript: stored verbatim on disk, including stable message `seq` IDs. Active user-source extraction excludes rewound rows. Original assistant/tool messages remain available through `research` operations `history_search` and `history_read` when tools/pipeline/history are enabled, including during verification.
2. Active user instructions: short histories retain up to 4096 serialized bytes directly. Longer histories use a model-selected set of at most 32 exact quotations and 8192 serialized bytes. Each quote must occur in the specified original user message. These pins are always in the active checkpoint, independent of retrieval ranking.
3. Working handoff: bounded summary of progress, historical observations, unresolved questions, and next steps. It has no authority to rewrite the source-backed instructions or turn historical values into requirements. Current-source reads still govern edits.
4. Recent context: the latest user message and up to four recent message/tool groups within the existing tail budget. Relevant older evidence can be retrieved as bounded original JSON pages.

Memory is session-local checkpoint state, separate from Builder's optional cross-session memory system. The checkpoint commits memory and working summary atomically after all validation and context-size checks. Failed or cancelled work leaves the previous context usable. The original history remains durable regardless of compaction success.

On initial extraction, all active original user messages are processed in bounded whole-message batches. On later compactions, only messages after the persisted source cursor are processed, alongside existing pins. Pins persist if omitted from an update. Identical quotation text is deduplicated with the newest source citation, so repeated reminders do not consume additional slots. Removing a pin requires an explicit retirement referencing that exact pin and a newer user quotation. The update and evidence are recorded in the attempt journal; previous checkpoints remain durable. Recovery only reads memory state from the dedicated slot immediately after the checkpoint’s system prefix, never a transcript message claiming to be internal memory (including a model message retained in a checkpoint tail).

Memory updates have separate `add` and `retire` lists. For example:

```json
{
  "add": [{"seq": 42, "quote": "Use timeout 25 instead of 10."}],
  "retire": [{
    "source": {"seq": 7, "quote": "Use timeout 10."},
    "evidence": {"seq": 42, "quote": "Use timeout 25 instead of 10."}
  }]
}
```

Code checks that additions occur in the supplied new user sources or are exact passages from already-validated pins. Retirement must name an existing pin and cite newer user evidence from the current batch. If a retired quote bundled several requirements, the selector can re-add exact passages for the unaffected requirements. The persisted state contains `through` (the last processed user seq) and `pins`; rejected or incomplete batches never advance the active checkpoint's cursor.

The source check proves quotation provenance, not semantic correctness. A model can select an incomplete or misleading span, omit an important requirement initially, or misinterpret a later message as superseding an earlier restriction. The prompt requires complete contextual quotations and forbids inferred permissions, but this is not a proof. Repeated-compaction behavioral evaluations are necessary. Historical retrieval likewise may miss synonyms or return insufficient evidence; the agent should retrieve exact pages, qualify uncertainty, and never invent unavailable details.

Active memory overflow rejects compaction instead of silently evicting constraints. An individual user message that cannot fit the extraction input budget also produces a clear failure without truncation. System instructions, denied/uncertain execution records, and the latest user request retain their existing protections and may independently exhaust the context budget. The design bounds ordinary archived-conversation growth; it cannot encode unlimited simultaneously active instructions in a finite prompt.

Source references informing the design:

- [Anthropic: effective context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents): compaction and persistent structured notes; aggressive compression can lose information.
- [Letta memory blocks](https://docs.letta.com/guides/agents/memory-blocks/): explicit bounded in-context memory.
- [LongMemEval](https://arxiv.org/abs/2410.10813): extraction, multi-session reasoning, temporal reasoning, knowledge updates, and abstention are distinct evaluation capabilities. Builder's synthetic tests are not a run of that benchmark.

Tool transport matters as much as storage. Research requests use a union of complete operation schemas, without sibling `properties` beside `anyOf`. Within each branch, the `operation` discriminator is serialized before its arguments. Builder explicitly enables serde_json's `preserve_order` in the tools crate; relying on a transitive build dependency does not preserve runtime ordering. On the tested llama.cpp endpoint, unordered serialization caused repeated `status` calls, and a flat-schema workaround produced invalid argument combinations. The ordered union restored successful history search/read calls while retaining strict per-operation validation. See [llama.cpp's JSON Schema limitations](https://github.com/ggml-org/llama.cpp/blob/master/grammars/README.md).

See [LLM_EVALUATIONS.md](LLM_EVALUATIONS.md) for deterministic and live tests.

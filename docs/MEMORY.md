# Builder memory

Memory stores short findings with source evidence, rather than indexing entire pasted files. It automatically loads relevant findings and the session's proposed next action into a bounded reference packet. Original conversations remain on disk. Memory can help avoid repeated discovery; a saved interpretation can still be wrong, and passing small evaluations does not establish correctness on a large repository task.

Compacted and rewound messages remain in the durable SQLite transcript outside
the active model prompt. With the history pipeline enabled, the model can search
that archive with `history_search` and page the exact original message with
`history_read`. This is retrieval-backed external memory: only the requested
page re-enters context, while the complete source record stays on disk.

## Enable and inspect

Memory stores its notes and vectors in local SQLite. Embedding generation defaults to an on-device CPU model; it does not use the chat endpoint or automatically select a saved remote embedding profile.

Open the CLI's slash menu and select **`/memory`**, then choose **Local embeddings**. This installs the model once, saves the selection, and applies it to that session. The menu also offers keyword-only memory and disabling memory while retaining records.

The equivalent terminal commands are:

```sh
builder memory enable --local
builder memory status
builder memory search 'Arena shield spawn configuration'
builder memory index
```

`builder memory enable` also defaults to local setup. Initial setup downloads approximately 91 MB of public model assets into `models/all-MiniLM-L6-v2` under Builder's application-data directory. On this Mac the directory is `~/Library/Application Support/dev.builder.builder/models/all-MiniLM-L6-v2`. Each file has a pinned SHA-256 digest and size. Completed downloads are reused, and invalid/partial files are never loaded. Setup verifies that the model can actually generate a vector before saving configuration.

After setup, inference loads only these files, with no model download, HTTP service, Python installation, or API key required. FastEmbed's online model-loading feature is disabled in the build. Missing or corrupt files produce a local setup diagnostic and keyword fallback, never a remote fallback. The model is [all-MiniLM-L6-v2 at a pinned revision](https://huggingface.co/Qdrant/all-MiniLM-L6-v2-onnx/tree/5f1b8cd78bc4fb444dd171e59b18f3a3af89a079), producing 384-dimensional normalized vectors with mean pooling and a 256-token input limit. CPU inference uses two threads. The implementation uses [FastEmbed](https://docs.rs/fastembed/6.0.3/fastembed/) with ONNX Runtime linked into the executable.

Use `builder memory enable --lexical` for keyword-only retrieval without installing a model. Remote embeddings remain an explicit opt-in through `builder memory enable --embedding-profile NAME`; set that model's required `--query-prefix`, `--document-prefix`, and optional `--embedding-revision`. Merely having an old embedding profile in configuration no longer enables remote requests. Local mode ignores remote profiles and their prefixes/credentials.

After changing settings through a separate terminal command, restart an already-running Builder session. Changes made through `/memory` apply immediately. Chat still uses the selected chat profile; this setting controls embedding generation.

`builder memory refresh SESSION_ID` runs a bounded extraction and embedding pass for an inactive saved session, using that session’s workspace and chat profile. It never invents replacement evidence for source that has changed.

Other commands include `memory list`, `memory get KEY [--revision N]`, `memory task SESSION_ID`, `memory remember KEY TEXT` for explicit global preferences, and `memory forget KEY [--user]`. Forgetting retains audit history. These notes never grant permissions or prove a task complete.

## What happens automatically

After a new successful source read, Builder can run a short tool-free extraction from up to sixteen recent durable source reads. Reads are selected independently of intervening shell output and commentary, then checked against current source hashes. Automatic extraction and pending-vector indexing run in a cancellable idle task only after the foreground answer is complete. They never run before a foreground model request or compaction, and a new user request cancels idle maintenance before contacting the chat model. At most one extraction generation runs per idle pass. In interactive chat, memory maintenance runs independently of the code embedding backlog and retries every thirty seconds while idle; `/status` shows its last result. Batches without fresh eligible source reads are skipped. Extraction has a ninety-second deadline and a 2,048-token output budget, and requests a JSON-mode response when the endpoint supports it. If the profile explicitly supplies the `enable_thinking` chat-template option, extraction disables it for this small derived JSON request; normal chat keeps its configured thinking setting. Incomplete or malformed output does not become a finding; prose or Markdown around an otherwise valid extraction object is tolerated. A batch's cursor advances only after its findings and task state are saved, so a failed or cancelled extraction is reconsidered by a later idle pass rather than permanently skipped. Maintenance failure never delays or prevents the foreground conversation. The extractor sees only user excerpts, fresh successful source-read excerpts, current findings and eligible read IDs, not an unbounded copy of the transcript. Extraction remains additional model work, but it no longer displays a foreground memory spinner or sits on the response latency path.

A finding requires one to eight successful `read_file` IDs. Each read includes a hash of the exact full file version read; the runtime verifies that version again before saving. Search validates hashes and unretracted evidence before returning a finding. A changed, missing, or rewound source yields only a stale location lead, excluding the old claim. Re-read the relevant range and update the same key with its current `expected_revision` to refresh it. Hash agreement proves version agreement, not the truth of the model's interpretation.

The session task state is separate: a proposed next action and open questions, produced by automatic extraction (the model no longer writes it directly; its own plan is the `todo_write` list). While the session has an unfinished todo list, the reference packet omits the extracted proposal so the model has one source for its next step. Recent durable tool-result excerpts supply execution evidence. New user instructions suppress older task proposals. Neither memory text nor a proposal grants permission or constitutes proof of completed work. Exact edits still need current source anchors.

Explicit memory search combines SQLite FTS5 keyword matches, exact keys/paths, and cosine vector rankings using reciprocal-rank fusion. Agent searches accept dense ranking only when its top score and top-versus-runner-up margin satisfy the `/settings` values `memory_min_similarity_percent` and `memory_min_margin_percent`; otherwise retrieval explicitly reports semantic abstention and keeps supported lexical matches. This calibration is model-fingerprint-aware at storage time but the default thresholds are a conservative starting policy, not a benchmark-derived guarantee. It considers at most 10,000 current records per checkout and checks the top twenty candidates. Explicit search returns up to eight notes/6,000 bytes. The automatic foreground reference packet uses immediate lexical ranking plus exact keys/paths so embedding initialization or endpoint latency cannot delay the conversation. The packet, including preferences and task state, is bounded to 6,000 bytes by removing whole optional entries. This is selection of derived memory, not deletion or truncation of conversation history. The packet participates in normal context accounting.

Local embeddings have a fifteen-second caller deadline, an 8,192-byte input limit, and fixed 384-dimensional output. Model initialization and inference are serialized; cancellation cannot queue additional blocking jobs behind an existing worker. Remote opt-in requests retain the three-second application deadline and bounded response validation. Missing vectors are a durable indexing queue: up to two current records are indexed per idle maintenance pass or via `memory index`. A timeout or endpoint error opens a circuit for the rest of that maintenance pass or explicit search. Idle indexing has an additional twelve-second deadline. Cancellation leaves missing vectors eligible for a later pass. An unavailable server does not destroy lexical memory. Vectors with another model namespace or incompatible dimensions are excluded.

## Persistence and permissions

Findings are scoped to the canonical checkout path. They can be reused by another session in that checkout, but do not leak into another checkout's retrieval. Global preferences require the user's explicit CLI command. Revisions use optimistic compare-and-swap; a stale writer cannot overwrite a newer revision. Forgetting is a tombstone and removes active search/vector entries. Rewinds deactivate task/evidence provenance without deleting audit records.

Memory search/get and derived finding upsert are internal memory operations authorized by enabling memory. They do not authorize filesystem edits, shell commands, or global preference changes. Forgetting requires the usual write approval. Model memory calls use the same durable claim/result journal as all tools: an interrupted call with uncertain execution is never automatically replayed. Multi-finding extraction validates its output first, then commits individual revision updates; interruption can leave some valid findings stored. The extraction cursor records the last completed batch. It advances only after a batch is saved; failed derived work is retried by a later idle pass, while original transcript rows remain intact. It does not claim an all-or-nothing transaction for the entire extraction batch.

Memory and transcripts are local plaintext SQLite data. Relevant findings are sent to the configured chat endpoint. Local embedding generation stays on-device; only explicit remote embedding mode sends finding text and search queries to its configured endpoint. Existing credential/header protections also apply to embeddings. Extractors are instructed not to store secrets; this is not a general-purpose secret detector.

## Evaluation

See [LLM_EVALUATIONS.md](LLM_EVALUATIONS.md). `BUILDER_EVAL_MEMORY=off` and `on` run the same fixtures, grading files and answers independently of model self-reports. Reports include all chat requests, including extraction and compaction, total time, failures, tool calls, repeated read bytes, and correctness. Unit/integration tests separately exercise source invalidation, revisions, rewind, isolation, failed extraction, vector validation, packet bounds, authentication, denial, and uncertain execution.

The four existing live fixtures are intentionally small. They test recovery and correction behavior, not success on the original Arena change or long-horizon coding competence. Repeat trials and larger held-out tasks are needed before claiming a reliable improvement.
